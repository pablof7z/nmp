// The Rust-handle -> Kotlin-`Flow` bridge -- the Kotlin counterpart of
// Query.swift's `NMPQuery`/`RowBridge`. This is the heart of the ergonomic
// layer: the ONLY place `com.nmp.sdk` folds the exact `FfiFrame` deltas into
// the accumulated snapshot a collector sees.
//
// PULL MODEL (#680): the engine owns a single-slot latest-state mailbox per
// observation; the wrapper is a thin pull adapter that awaits
// `NmpRowStream.next()` in a loop and folds each delta frame into the
// running snapshot. `null` from `next()` is the terminal signal (cancel /
// engine shutdown / producer drop). There is NO callback observer, NO
// dedicated drain thread, and NO native-task capacity concept anymore.
//
// TEARDOWN is COLLECTION-SCOPE-TIED: `handle.cancel()` runs in a `finally`,
// which fires the moment the collecting coroutine is cancelled or the flow
// completes. Cancelling the collecting coroutine also drops the in-flight
// Rust `next()` future; the Rust single-reader guard is released on that
// future's `Drop`, so a torn-down collection cannot brick the handle. This
// is the deterministic teardown #46's bounded-latest-state contract needs --
// not the JVM `Cleaner`, which only runs once GC actually collects.
//
// CONFLATION lives in the engine mailbox now, so there is deliberately NO
// `.conflate()` on this side: a slow collector simply does not call `next()`
// again until it is ready, and the engine folds intermediate reducer emits
// into the single retained slot (exact deltas are rebased onto the last
// delivered frame). Backpressure is expressed by not pulling.
package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiRowDelta
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpRowStream

/** A live, detachable query (`nmp_engine`'s read noun). The returned `Flow`
 * is the PRIMARY read handle -- `collect` it directly; there is no
 * container or provider object required around it (mirrors `NMPQuery`'s own
 * doc / the M4 plan §7 canary).
 *
 * Each element is the full accumulated snapshot (`RowBatch`), never a bare
 * delta: the pull loop folds `Added`/`Removed`/`SourcesGrew` deltas
 * internally so a collector never has to. The cold `Flow` opens a fresh
 * single-consumer stream on each collection and withdraws it (via
 * `handle.cancel()`) the moment collection ends -- see this file's header
 * for why that, not UniFFI's generated `Cleaner`, is the correct mapping. */
fun observeQuery(engine: NmpEngineInterface, filter: NMPFilter): Flow<RowBatch> =
    rowFlow { nmpRethrowing { engine.observe(filter.toFfi(), null) } }

/** #107: the explicit-`NMPDemand` entry point -- the constructor to reach
 * for once [observeQuery]'s implicit `AuthorOutboxes`/`Public` default
 * isn't enough: declaring `NMPSourceAuthority.Pinned` wire authority, a
 * non-default `NMPAccessContext`, or a non-`Agnostic` `NMPCacheMode`. Same
 * pull-loop/accumulation/teardown shape as the `NMPFilter` overload above. */
fun observeQuery(engine: NmpEngineInterface, demand: NMPDemand): Flow<RowBatch> =
    rowFlow { nmpRethrowing { engine.observeDemand(demand.toFfi(), null) } }

/** Shared pull loop for the unbounded (delta-folding) row observations.
 * `open` is the ONE difference between the `NMPFilter` and `NMPDemand` entry
 * points (which `NmpEngineInterface` verb actually opens the subscription),
 * run lazily per collection so the `Flow` stays cold. */
private fun rowFlow(open: () -> NmpRowStream): Flow<RowBatch> =
    flow {
        val handle = open()
        try {
            // Insertion-ordered accumulation: `order` tracks arrival order,
            // `byId` the current value for each still-live row -- same shape
            // as Swift's `RowBridge`. `com.nmp.sdk` does mechanics only
            // (accumulate what the engine says is live); ordering/rendering
            // policy is an app concern (feed doctrine), not this loop's.
            val order = mutableListOf<String>()
            val byId = mutableMapOf<String, Row>()
            while (true) {
                // These observations are opened with `window = null`, so
                // every delivered `frame.window` is null and `frame.deltas`
                // is the exact transition rebased onto the last delivered
                // Rust frame -- fold it. (Windowed observations take the
                // other arm of the one frame vocabulary: authoritative
                // snapshots, no folding -- see Window.kt.)
                val frame = nmpRethrowingAsync { handle.next() } ?: break
                for (delta in frame.deltas) applyRowDelta(order, byId, delta)
                val snapshot = order.mapNotNull { byId[it] }
                emit(RowBatch(snapshot, AcquisitionEvidence.from(frame.evidence)))
            }
        } finally {
            handle.cancel()
        }
    }

/**
 * The accumulator's per-delta step, extracted as a pure function so it is
 * directly unit-testable (#105's `SourcesGrew` replace-in-place proof)
 * without driving the coroutine/`Flow` machinery around it. Mutates
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
