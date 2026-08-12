package com.nmp.qualification.consumer

import com.nmp.sdk.DiagnosticsSnapshot
import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPEngine
import kotlinx.coroutines.flow.Flow

/** Clean consumer: no generated package, native-loader, or repository path. */
class QualificationConsumer(
    private val engine: NMPEngine,
) {
    fun diagnostics(): Flow<DiagnosticsSnapshot> = engine.observeDiagnostics()

    fun persistentConfiguration(path: String): NMPConfig =
        NMPConfig(storePath = path, maxRelays = 2u)
}
