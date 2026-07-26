package com.nmp.qualification.consumer

import com.nmp.sdk.*
import kotlinx.coroutines.flow.Flow

/**
 * A clean external compile consumer. It depends on the published AAR
 * coordinate and names only the supported ergonomic package.
 */
class QualificationConsumer(
    private val engine: NMPEngine,
) {
    fun diagnostics(): Flow<DiagnosticsSnapshot> = engine.observeDiagnostics()

    fun persistentConfiguration(path: String): NMPConfig =
        NMPConfig(storePath = path, maxRelays = 2u)
}
