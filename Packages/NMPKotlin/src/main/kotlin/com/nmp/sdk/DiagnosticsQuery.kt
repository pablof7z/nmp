// The Rust-channel -> Kotlin-`Flow` bridge for the diagnostic surface,
// mirroring Query.kt's `observeQuery`/`RowObserver` bridge exactly. This is
// the ONLY place `com.nmp.sdk` holds a `DiagnosticsObserver` conformance.
// Mirrors DiagnosticsQuery.swift.

package com.nmp.sdk

import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import uniffi.nmp_ffi.DiagnosticsObserver
import uniffi.nmp_ffi.FfiDiagnosticsSnapshot
import uniffi.nmp_ffi.NmpEngineInterface

/** A live diagnostics stream (`nmp_engine`'s read-only diagnostic
 * projection). Collect the returned `Flow` directly, same discipline as
 * `observeQuery`'s. Each element is the CURRENT engine-global
 * `DiagnosticsSnapshot` -- never a delta (there is nothing to accumulate
 * here: every snapshot is already the full current picture). `.conflate()`
 * mirrors `observeQuery`'s own bounded-latest-state discipline; demand
 * teardown is collection-scope-tied via `awaitClose`, same reasoning as
 * Query.kt's header finding. */
fun observeDiagnostics(engine: NmpEngineInterface): Flow<DiagnosticsSnapshot> =
    callbackFlow {
        val observer =
            object : DiagnosticsObserver {
                override fun onSnapshot(snapshot: FfiDiagnosticsSnapshot) {
                    trySendBlocking(DiagnosticsSnapshot.from(snapshot))
                }

                override fun onClosed() {
                    close()
                }
            }

        val handle = engine.observeDiagnostics(observer)

        awaitClose { handle.cancel() }
    }.conflate()
