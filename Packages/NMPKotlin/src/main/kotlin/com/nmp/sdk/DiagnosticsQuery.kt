// The Rust-handle -> Kotlin-`Flow` bridge for the diagnostic surface,
// mirroring Query.kt's `observeQuery` pull loop exactly. Mirrors
// DiagnosticsQuery.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.NmpEngineInterface

/** A live diagnostics stream (`nmp_engine`'s read-only diagnostic
 * projection). Collect the returned `Flow` directly, same discipline as
 * `observeQuery`'s. Each element is the CURRENT engine-global
 * `DiagnosticsSnapshot` -- never a delta (there is nothing to accumulate
 * here: every snapshot is already the full current picture). The first
 * `next()` yields the current snapshot immediately, then a fresh one on
 * every coverage change; the engine's latest-state mailbox conflates
 * intermediate snapshots for a slow collector. Demand teardown is
 * collection-scope-tied via `handle.cancel()` in a `finally`, same reasoning
 * as Query.kt's header. */
fun observeDiagnostics(engine: NmpEngineInterface): Flow<DiagnosticsSnapshot> =
    flow {
        val handle = nmpRethrowing { engine.observeDiagnostics() }
        try {
            while (true) {
                val snapshot = nmpRethrowingAsync { handle.next() } ?: break
                emit(DiagnosticsSnapshot.from(snapshot))
            }
        } finally {
            handle.cancel()
        }
    }
