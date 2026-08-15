// #1624: the missing-capability identity is a VALUE, and it is the same value
// on both SDKs.
//
// `MissingReplaceableCapabilityTests.swift` is this file's exact mirror: the
// same two 16-byte compiled-capability identifiers, the same expected
// lowercase-hex spellings, the same assertions about equality. Cross-SDK
// agreement is proven by the two suites agreeing on those literals; the
// constants below and in the Swift file must be edited together.
//
// The falsified bug: the FFI hands `program`/`format` over as `ByteArray`,
// and a Kotlin `data class` holding a `ByteArray` gets reference-based
// `equals`/`hashCode` plus a mutable field. Two errors naming the same missing
// capability compared UNEQUAL, hashed apart, printed `[B@1f2e3d`, and a caller
// could edit a value already handed out.

package com.nmp.sdk

import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiException
import kotlin.test.assertEquals
import kotlin.test.assertNotEquals
import kotlin.test.assertTrue

class MissingReplaceableCapabilityTest {
    @Test
    fun ffiBytesProjectAsCanonicalLowercaseHex() {
        val error = translate(PROGRAM_BYTES, FORMAT_BYTES)

        assertEquals(PROGRAM_HEX, error.programHex)
        assertEquals(FORMAT_HEX, error.formatHex)
        assertEquals(32, error.programHex.length)
        assertEquals(32, error.formatHex.length)
    }

    @Test
    fun twoErrorsNamingTheSameCapabilityAreOneValue() {
        val first = translate(PROGRAM_BYTES, FORMAT_BYTES)
        val second = translate(PROGRAM_BYTES, FORMAT_BYTES)

        // The exact defect being pinned: the FFI's own `ByteArray` identities
        // compare by reference, so an error that carried them unchanged made
        // two reports of ONE missing capability two different values.
        assertNotEquals(
            PROGRAM_BYTES.copyOf(),
            PROGRAM_BYTES.copyOf(),
            "a ByteArray identity compares by reference; that is what hex replaces",
        )

        assertTrue(first !== second, "the two errors must be separately constructed instances")
        assertEquals(first, second)
        assertEquals(first.hashCode(), second.hashCode())
        assertEquals(1, setOf(first, second).size, "one capability is one set member")
    }

    @Test
    fun distinctCapabilitiesRemainDistinctKeys() {
        val missing = translate(PROGRAM_BYTES, FORMAT_BYTES)
        val other = translate(FORMAT_BYTES, PROGRAM_BYTES)

        assertNotEquals(missing, other)
        val byCapability = mapOf(missing to "missing", other to "other")
        assertEquals(2, byCapability.size)
        assertEquals(
            "missing",
            byCapability[NMPError.MissingReplaceableCapability(PROGRAM_HEX, FORMAT_HEX)],
            "a freshly built key must find the retained entry",
        )
    }

    @Test
    fun theRenderedValueShowsTheIdentityItNames() {
        val rendered = translate(PROGRAM_BYTES, FORMAT_BYTES).toString()

        assertTrue(rendered.contains(PROGRAM_HEX), rendered)
        assertTrue(rendered.contains(FORMAT_HEX), rendered)
    }

    private fun translate(
        program: ByteArray,
        format: ByteArray,
    ): NMPError.MissingReplaceableCapability {
        // `copyOf()` so nothing downstream can share a buffer with the caller.
        val translated =
            NMPError.from(
                FfiException.MissingReplaceableCapability(program.copyOf(), format.copyOf()),
            )
        return translated as NMPError.MissingReplaceableCapability
    }
}

// Exercises a leading zero byte, the 0x0f/0x10 nibble boundary, the sign
// boundary at 0x7f/0x80, and 0xff -- every way a naive hex encoder goes wrong.
private val PROGRAM_BYTES =
    byteArrayOf(
        0x00, 0x01, 0x0f, 0x10,
        0x7f, 0x80.toByte(), 0xfe.toByte(), 0xff.toByte(),
        0x2a, 0x3b, 0x4c, 0x5d,
        0x6e, 0x7f, 0x80.toByte(), 0x91.toByte(),
    )
private const val PROGRAM_HEX = "00010f107f80feff2a3b4c5d6e7f8091"

private val FORMAT_BYTES =
    byteArrayOf(
        0xff.toByte(), 0xee.toByte(), 0xdd.toByte(), 0xcc.toByte(),
        0xbb.toByte(), 0xaa.toByte(), 0x99.toByte(), 0x88.toByte(),
        0x77, 0x66, 0x55, 0x44,
        0x33, 0x22, 0x11, 0x00,
    )
private const val FORMAT_HEX = "ffeeddccbbaa99887766554433221100"
