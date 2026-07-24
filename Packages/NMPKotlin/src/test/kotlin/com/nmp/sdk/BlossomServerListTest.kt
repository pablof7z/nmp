package com.nmp.sdk

import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

class BlossomServerListTest {
    private fun row(): Row =
        Row(
            id = "11".repeat(32),
            pubkey = "22".repeat(32),
            createdAt = 10u,
            kind = 10063u,
            tags =
                listOf(
                    listOf("server", "http://127.0.0.1:3000"),
                    listOf("server"),
                    listOf("server", "ftp://invalid.example"),
                    listOf("server", "https://8.8.8.8"),
                ),
            content = "not-used",
            sig = "33".repeat(64),
            sources = listOf("wss://relay.example"),
        )

    @Test
    fun demandAndDecodeKeepOrdinaryQueryAndMalformedEvidence() {
        val demand = blossomServerListDemand()
        assertEquals(listOf<UShort>(10063u), demand.selection.kinds)
        assertEquals(NMPSourceAuthority.AuthorOutboxes, demand.source)
        assertEquals(NMPAccessContext.Public, demand.access)

        val list = decodeBlossomServerList(row())
        assertEquals(
            listOf("http://127.0.0.1:3000/", "https://8.8.8.8/"),
            list.servers,
        )
        assertEquals(4uL, list.serverTagCount)
        assertEquals(listOf<ULong>(1u, 2u), list.malformedEntries.map { it.tagIndex })
        assertTrue(list.malformedEntries[0].error is BlossomServerListEntryError.MissingUrl)
        val invalid = list.malformedEntries[1].error
        if (invalid !is BlossomServerListEntryError.InvalidUrl) {
            throw AssertionError("invalid scheme must remain typed")
        }
        assertEquals(
            BlossomServerUrlError.UnsupportedScheme("ftp"),
            (invalid as BlossomServerListEntryError.InvalidUrl).error,
        )
        assertTrue(list.hasUnexpectedContent)
        assertFalse(list.isSpecCompliant)
    }

    @Test
    fun signedLocalHostNeverMintsAdmissionAndProvenanceOrderSurvives() =
        runBlocking {
            val list = decodeBlossomServerList(row())
            val evidence =
                BlossomClient().qualifyServerCandidates(
                    policy = BlossomServerCandidatePolicy.SIGNED_LIST_THEN_OPERATOR,
                    operatorServerUrls = listOf("https://1.1.1.1"),
                    signedList = list,
                )
            assertEquals(3, evidence.size)
            assertEquals(
                listOf(
                    BlossomServerCandidateSource.SIGNED_LIST,
                    BlossomServerCandidateSource.SIGNED_LIST,
                    BlossomServerCandidateSource.OPERATOR_CONFIG,
                ),
                evidence.map { it.source },
            )
            assertEquals(
                BlossomServerAdmission.LocalHostNotAdmitted("127.0.0.1"),
                evidence[0].admission,
            )
            val signed = evidence[1].admission
            if (signed !is BlossomServerAdmission.Admitted) {
                throw AssertionError("public signed endpoint should qualify")
            }
            assertFalse((signed as BlossomServerAdmission.Admitted).operatorLocalOverride)
            val operator = evidence[2].admission
            if (operator !is BlossomServerAdmission.Admitted) {
                throw AssertionError("public operator endpoint should qualify")
            }
            assertFalse((operator as BlossomServerAdmission.Admitted).operatorLocalOverride)
        }
}
