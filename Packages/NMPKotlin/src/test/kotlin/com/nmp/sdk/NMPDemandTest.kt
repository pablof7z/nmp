package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test

// A construction/round-trip test of the ergonomic Demand descriptor (#107).
// No network -- this only proves the Kotlin-value <-> Ffi-value conversion
// is lossless for every SourceAuthority/AccessContext/CacheMode/Freshness case.
class NMPDemandTest {
    @Test
    fun authorOutboxesSourceRoundTrips() {
        val demand = NMPDemand(selection = NMPFilter(kinds = listOf(1u)), source = NMPSourceAuthority.AuthorOutboxes)
        val ffi = demand.toFfi()
        assertEquals(uniffi.nmp_ffi.FfiSourceAuthority.AuthorOutboxes, ffi.source)
        assertEquals(uniffi.nmp_ffi.FfiAccessContext.Public, ffi.access)
        assertEquals(uniffi.nmp_ffi.FfiCacheMode.AGNOSTIC, ffi.cache)
        assertEquals(uniffi.nmp_ffi.FfiFreshness.Live, ffi.freshness)
        assertEquals(demand, NMPDemand.from(ffi))
    }

    @Test
    fun pinnedSourceRoundTripsWithStrictCache() {
        val demand =
            NMPDemand(
                selection = NMPFilter(kinds = listOf(1u)),
                source = NMPSourceAuthority.Pinned(setOf("wss://relay.example.com")),
                cache = NMPCacheMode.Strict,
            )
        val ffi = demand.toFfi()
        val source = ffi.source as uniffi.nmp_ffi.FfiSourceAuthority.Pinned
        assertEquals(listOf("wss://relay.example.com"), source.relays)
        assertEquals(uniffi.nmp_ffi.FfiCacheMode.STRICT, ffi.cache)
        assertEquals(demand, NMPDemand.from(ffi))
    }

    @Test
    fun cacheModeDefaultsToAgnosticWhenUnspecified() {
        val demand = NMPDemand(selection = NMPFilter(kinds = listOf(1u)), source = NMPSourceAuthority.Public)
        assertEquals(NMPCacheMode.Agnostic, demand.cache)
        assertEquals(NMPAccessContext.Public, demand.access)
    }

    @Test
    fun nip42AccessRoundTripsWithExactExpectedPublicKey() {
        val demand =
            NMPDemand(
                selection = NMPFilter(kinds = listOf(1u)),
                source = NMPSourceAuthority.Pinned(setOf("wss://relay.example.com")),
                access = NMPAccessContext.Nip42("a".repeat(64)),
            )
        assertEquals(demand, NMPDemand.from(demand.toFfi()))
    }

    @Test
    fun freshnessRoundTripsEveryWholeSecondVariant() {
        listOf(
            NMPFreshness.Live,
            NMPFreshness.MaxAge(14_400uL),
            NMPFreshness.CacheOnly,
        ).forEach { freshness ->
            val demand =
                NMPDemand(
                    selection = NMPFilter(kinds = listOf(0u)),
                    source = NMPSourceAuthority.AuthorOutboxes,
                    freshness = freshness,
                )
            assertEquals(demand, NMPDemand.from(demand.toFfi()))
        }
    }
}
