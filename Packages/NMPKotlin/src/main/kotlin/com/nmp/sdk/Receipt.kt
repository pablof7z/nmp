// The write noun's receipt stream, mirroring Query.kt's bridge pattern but
// for `ReceiptObserver`/`WriteStatus` instead of `RowObserver`/`RowDelta`.
// Mirrors Receipt.swift.

package com.nmp.sdk

import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import uniffi.nmp_ffi.FfiWriteStatus
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.ReceiptObserver

/** Enqueue a write. Returns a `Flow<WriteStatus>` streaming everything that
 * happens to it after acceptance (M4 plan §9 -- `publish` is a one-shot
 * enqueue call, the STREAM is where convergence is observed; mirrors
 * `NMPEngine.publish`'s doc). Unlike `observeQuery`, there is no explicit
 * handle to cancel -- `nmp-engine`'s own `publish` receiver has no
 * unsubscribe concept (a write, once enqueued, runs to its own terminal
 * regardless of whether anything is still listening); `awaitClose` here
 * only stops draining the Rust-side channel into this flow, matching
 * `ReceiptBridge`'s equivalent (implicit) discipline on the Swift side. */
fun publishReceipt(engine: NmpEngineInterface, intent: WriteIntent): Flow<WriteStatus> =
    callbackFlow {
        val observer =
            object : ReceiptObserver {
                override fun onStatus(status: FfiWriteStatus) {
                    trySendBlocking(WriteStatus.from(status))
                }
            }

        nmpRethrowing { engine.publish(intent.toFfi(), observer) }

        awaitClose { }
    }
