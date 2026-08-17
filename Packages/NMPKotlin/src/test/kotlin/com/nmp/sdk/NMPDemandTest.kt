package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNotEquals
import org.junit.jupiter.api.Test

// A construction/round-trip test of the ergonomic Demand descriptor (#107).
// No network -- this only proves the Kotlin-value <-> Ffi-value conversion
// is lossless for every ReadRouting/AccessContext/CacheMode/Freshness case.
class NMPDemandTest {
    @Test
    fun aDemandThatNamesNoRoutingRoundTripsAsAuto() {
        val demand = NMPDemand(selection = NMPFilter(kinds = listOf(1u)))
        val ffi = demand.toFfi()
        assertEquals(uniffi.nmp_ffi.FfiReadRouting.Auto, ffi.routing)
        assertEquals(uniffi.nmp_ffi.FfiAccessContext.Public, ffi.access)
        assertEquals(uniffi.nmp_ffi.FfiCacheMode.AGNOSTIC, ffi.cache)
        assertEquals(uniffi.nmp_ffi.FfiFreshness.Live, ffi.freshness)
        assertEquals(demand, NMPDemand.from(ffi))
    }

    @Test
    fun explicitRoutingRoundTripsWithStrictCache() {
        val demand =
            NMPDemand(
                selection = NMPFilter(kinds = listOf(1u)),
                routing = NMPReadRouting.Explicit(listOf("wss://relay.example.com")),
                cache = NMPCacheMode.Strict,
            )
        val ffi = demand.toFfi()
        val routing = ffi.routing as uniffi.nmp_ffi.FfiReadRouting.Explicit
        assertEquals(listOf("wss://relay.example.com"), routing.relays)
        assertEquals(uniffi.nmp_ffi.FfiCacheMode.STRICT, ffi.cache)
        assertEquals(demand, NMPDemand.from(ffi))
    }

    @Test
    fun cacheModeDefaultsToAgnosticWhenUnspecified() {
        val demand = NMPDemand(selection = NMPFilter(kinds = listOf(1u)))
        assertEquals(NMPCacheMode.Agnostic, demand.cache)
        assertEquals(NMPAccessContext.Public, demand.access)
    }

    @Test
    fun nip42AccessRoundTripsWithExactExpectedPublicKey() {
        val demand =
            NMPDemand(
                selection = NMPFilter(kinds = listOf(1u)),
                routing = NMPReadRouting.Explicit(listOf("wss://relay.example.com")),
                access = NMPAccessContext.Nip42("a".repeat(64)),
            )
        assertEquals(demand, NMPDemand.from(demand.toFfi()))
    }

    @Test
    fun derivedInnerFullDemandRoundTripsEveryPolicyIndependently() {
        val inner =
            NMPDemand(
                selection =
                    NMPFilter(
                        kinds = listOf(3u),
                        authors = NMPBinding.Reactive(NMPIdentityField.ActivePubkey),
                    ),
                routing = NMPReadRouting.Explicit(listOf("wss://inner.example.com")),
                access = NMPAccessContext.Nip42("a".repeat(64)),
                cache = NMPCacheMode.Strict,
                freshness = NMPFreshness.MaxAge(600uL),
            )
        val filter =
            NMPFilter(
                kinds = listOf(1u),
                authors = NMPBinding.Derived(inner, NMPSelector.Tag("p")),
            )

        val ffi = filter.toFfi()
        val derived = ffi.authors as uniffi.nmp_ffi.FfiBinding.Derived
        assertEquals(inner, NMPDemand.from(derived.derived.inner()))
        assertEquals(filter, NMPFilter.from(ffi))

        val publicInner = inner.copy(access = NMPAccessContext.Public)
        val sameSelectionDifferentContext =
            NMPFilter(
                kinds = listOf(1u),
                authors = NMPBinding.Derived(publicInner, NMPSelector.Tag("p")),
            )
        assertNotEquals(filter, sameSelectionDifferentContext)
        assertEquals(sameSelectionDifferentContext, NMPFilter.from(sameSelectionDifferentContext.toFfi()))
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
                    freshness = freshness,
                )
            assertEquals(demand, NMPDemand.from(demand.toFfi()))
        }
    }
}
