// The Rust-channel -> Kotlin-`Flow` bridge -- the Kotlin counterpart of
// Query.swift's `NMPQuery`/`RowBridge`. This is the heart of the ergonomic
// layer: the ONLY place `com.nmp.sdk` holds a `RowObserver` conformance,
// and the ONLY place a callback thread's mutation touches shared state.
//
// FINDING (#40): the two nouns port cleanly to `Flow`, but demand-teardown
// does NOT map onto the same mechanism Swift uses. Swift's `NMPQuery` ties
// withdrawal to ARC `deinit` (prompt, refcount-driven); the naive Kotlin
// mirror would be UniFFI's own generated `Disposable`/`Cleaner` machinery
// (`NmpQueryHandle` registers a `java.lang.ref.Cleaner` cleanup action) --
// but the JVM only runs a `Cleaner` action once GC actually collects the
// object, which is unbounded and NOT a substitute for demand withdrawal
// (#46's bounded-latest-state contract needs teardown to happen, not
// eventually-maybe-happen). `callbackFlow`'s `awaitClose` is the correct
// mapping instead: it fires deterministically the moment the collecting
// coroutine is cancelled or completes -- the same "collection-scope-ended"
// edge `docs/builder/30-platform-guides.md`'s PLANNED-shape section
// prescribed (the `WhileSubscribed` refcount edge, one layer up, when a
// caller shares this flow via `stateIn`/`shareIn`). `cancel()` on the
// returned handle exists for the same reason Swift's does: an explicit
// early teardown a caller doesn't want to wait on scope-exit for.
package com.nmp.sdk

import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import uniffi.nmp_ffi.FfiFrame
import uniffi.nmp_ffi.FfiRowDelta
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpQueryHandle
import uniffi.nmp_ffi.RowObserver
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

/** A live, detachable query (`nmp_engine`'s read noun). The returned `Flow`
 * is the PRIMARY read handle -- `collect` it directly; there is no
 * container or provider object required around it (mirrors `NMPQuery`'s own
 * doc / the M4 plan §7 canary).
 *
 * Each element is the full accumulated snapshot (`RowBatch`), never a bare
 * delta: the bridge accumulates `Added`/`Removed` deltas internally so a
 * collector never has to. `.conflate()` gives the same "latest-wins, never
 * a growing backlog" delivery discipline as Swift's
 * `AsyncStream(bufferingPolicy: .bufferingNewest(1))` + `FrameCoalescer`
 * pair (#17/docs/known-gaps.md) -- Kotlin's own primitive already expresses
 * bounded-latest-state delivery natively, so no hand-rolled coalescer is
 * needed on this side (a genuine platform-idiom difference worth recording,
 * not a gap: `conflate()` drops on backpressure the instant a collector
 * falls behind, whereas `FrameCoalescer` throttles to a fixed ~16ms window
 * even when a collector is keeping up -- the Kotlin version is reactive
 * rather than time-sliced).
 *
 * Demand teardown is COLLECTION-SCOPE-TIED: withdrawal happens in
 * `awaitClose`, which fires the moment the collecting coroutine is
 * cancelled or the flow completes -- see this file's header finding for why
 * that, not UniFFI's generated `Cleaner`, is the correct mapping. */
fun observeQuery(engine: NmpEngineInterface, filter: NMPFilter): Flow<RowBatch> =
    observeRows { observer -> nmpRethrowing { engine.observe(filter.toFfi(), null, observer) } }

/** #107: the explicit-`NMPDemand` entry point -- the constructor to reach
 * for once [observeQuery]'s implicit `AuthorOutboxes`/`Public` default
 * isn't enough: declaring `NMPSourceAuthority.Pinned` wire authority, a
 * non-default `NMPAccessContext`, or a non-`Agnostic` `NMPCacheMode`. Same
 * bridge/accumulation/teardown shape as the `NMPFilter` overload above. */
fun observeQuery(engine: NmpEngineInterface, demand: NMPDemand): Flow<RowBatch> =
    observeRows { observer -> nmpRethrowing { engine.observeDemand(demand.toFfi(), null, observer) } }

/** Shared bridge/accumulation setup: `subscribe` is the ONE difference
 * between the `NMPFilter` and `NMPDemand` entry points (which
 * `NmpEngineInterface` verb actually opens the subscription). */
private fun observeRows(subscribe: (RowObserver) -> NmpQueryHandle): Flow<RowBatch> =
    callbackFlow {
        val lock = ReentrantLock()
        // Insertion-ordered accumulation: `order` tracks arrival order,
        // `byId` the current value for each still-live row -- same shape as
        // Swift's `RowBridge`. `com.nmp.sdk` does mechanics only (accumulate
        // what the engine says is live); ordering/rendering policy is an
        // app concern (feed doctrine), not this bridge's.
        val order = mutableListOf<String>()
        val byId = mutableMapOf<String, Row>()

        val observer =
            object : RowObserver {
                // These subscriptions are opened with `window = null`, so
                // every delivered `frame.window` is null and `frame.deltas`
                // is the exact lossless transition -- fold it. (Windowed
                // observations take the other arm of the one frame
                // vocabulary: authoritative snapshots, no folding -- see
                // Window.kt's `WindowBridge`.)
                override fun onFrame(frame: FfiFrame) {
                    val snapshot =
                        lock.withLock {
                            for (delta in frame.deltas) applyRowDelta(order, byId, delta)
                            order.mapNotNull { byId[it] }
                        }
                    trySendBlocking(RowBatch(snapshot, AcquisitionEvidence.from(frame.evidence)))
                }

                override fun onClosed() {
                    close()
                }
            }

        val handle: NmpQueryHandle = subscribe(observer)

        awaitClose { handle.cancel() }
    }.conflate()

/**
 * The accumulator's per-delta step, extracted as a pure function so it is
 * directly unit-testable (#105's `SourcesGrew` replace-in-place proof)
 * without driving the coroutine/`callbackFlow` machinery around it. Mutates
 * `order`/`byId` in place -- identical semantics to `RowBridge.onFrame`'s
 * Swift counterpart.
 */
internal fun applyRowDelta(
    order: MutableList<String>,
    byId: MutableMap<String, Row>,
    delta: FfiRowDelta,
) {
    when (delta) {
        is FfiRowDelta.Added -> {
            val row = Row.from(delta.row)
            if (!byId.containsKey(row.id)) order.add(row.id)
            byId[row.id] = row
        }
        is FfiRowDelta.SourcesGrew -> {
            // #105: the SAME row already matched; only its relay-provenance
            // set grew. Replace in place -- `order` is untouched, this is
            // never an insertion.
            byId[delta.id]?.let { existing ->
                byId[delta.id] = existing.copy(sources = delta.sources)
            }
        }
        is FfiRowDelta.Removed -> {
            if (byId.remove(delta.id) != null) order.remove(delta.id)
        }
    }
}
