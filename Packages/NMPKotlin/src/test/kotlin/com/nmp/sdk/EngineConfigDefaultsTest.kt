package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.NmpEngineConfig

class EngineConfigDefaultsTest {
    @Test
    fun ergonomicDefaultsComeFromGeneratedBoundary() {
        val generated = NmpEngineConfig()
        val ergonomic = NMPConfig()

        assertEquals(generated.storePath, ergonomic.storePath)
        assertEquals(generated.indexerRelays, ergonomic.indexerRelays)
        assertEquals(generated.appRelays, ergonomic.appRelays)
        assertEquals(generated.fallbackRelays, ergonomic.fallbackRelays)
        assertEquals(generated.allowedLocalRelayHosts, ergonomic.allowedLocalRelayHosts)
        assertEquals(generated.maxRelays, ergonomic.maxRelays)
        assertEquals(generated.maxAuthCapabilities, ergonomic.maxAuthCapabilities)
    }

    @Test
    fun explicitZeroKeepsItsDistinctFieldSemantics() {
        val config = NMPConfig(maxRelays = 0u, maxAuthCapabilities = 0u).toFfi()

        assertEquals(0u, config.maxRelays)
        assertEquals(0u, config.maxAuthCapabilities)
    }
}
