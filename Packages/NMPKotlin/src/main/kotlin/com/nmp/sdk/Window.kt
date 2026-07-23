// Windowing as a POLICY on the read noun (#485) -- not a parallel noun.
// `NMPEngine.observe(filter)` / `observe(demand)` stay the unbounded delta
// observations; passing a [Window] to the SAME verb opens a bounded
// newest-first observation instead, delivered as full snapshots through
// [NMPQuery.frames] and grown declaratively via [NMPQuery.requestRows].
//
// Delivery mode is DERIVED from boundedness, never a knob:
// - Unbounded observations have no ceiling, so redelivering the full row
//   set on every change is the O(rows^2) class -- they stream exact rebased
//   deltas (Query.kt) and each element is the accumulated snapshot.
// - Windowed observations are bounded by `max`, so a full snapshot per
//   frame is cheap AND makes every frame self-contained -- the engine's
//   latest-state mailbox can conflate intermediate frames with zero
//   information loss, which a delta stream never could.
//
// PULL MODEL (#680): [frames] is a thin pull adapter over the windowed
// [NmpRowStream] -- it awaits `next()`, maps each authoritative frame to a
// full [RowBatch], and withdraws the observation (`handle.cancel()`) in a
// `finally` when collection ends. No callback observer, no drain thread, no
// retain-before-collection bookkeeping (the engine mailbox holds the latest
// snapshot until the first pull).

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiFrame
import uniffi.nmp_ffi.FfiRequestRowsException
import uniffi.nmp_ffi.FfiWindow
import uniffi.nmp_ffi.FfiWindowLoad
import uniffi.nmp_ffi.NmpRowStream
import java.util.concurrent.atomic.AtomicBoolean

/** Window policy on the read noun. Exactly one policy exists today; future
 * policies are new variants of this class, never new observe verbs. */
sealed class Window {
    /** Bounded newest-first window: starts with [initial] canonical rows,
     * grows only by explicit [NMPQuery.requestRows], never above [max].
     * Both counts must be non-zero and `initial <= max` -- violations throw
     * typed [NMPError]s at `observe` time. */
    data class Expandable(val initial: ULong, val max: ULong) : Window()

    internal fun toFfi(): FfiWindow =
        when (this) {
            is Expandable -> FfiWindow.Expandable(initial = initial, max = max)
        }
}

/** Mechanical growth state of an expandable window, delivered as a FACT on
 * every windowed [RowBatch] -- never thrown. In particular [AtBound] is not
 * an error: hitting the declared ceiling is an ordinary, expected outcome
 * of a bounded read, and a fact in the frame is something a UI can render
 * ("end of window") where an exception could only be swallowed.
 *
 * There is deliberately no Complete/End/Synced variant: `Returned(added=0)`
 * only means the planned advance added no canonical row -- consult the
 * frame's per-source [AcquisitionEvidence] for what that absence proves. */
sealed class WindowLoad {
    object Idle : WindowLoad()

    object Requesting : WindowLoad()

    data class Returned(val added: ULong) : WindowLoad()

    data class AtBound(val max: ULong) : WindowLoad()

    companion object {
        internal fun from(ffi: FfiWindowLoad): WindowLoad =
            when (ffi) {
                is FfiWindowLoad.Idle -> Idle
                is FfiWindowLoad.Requesting -> Requesting
                is FfiWindowLoad.Returned -> Returned(ffi.added)
                is FfiWindowLoad.AtBound -> AtBound(ffi.max)
            }
    }
}

/** Exact failures from [NMPQuery.requestRows]. Only genuine inability to
 * grow is an error -- being at the bound is a [WindowLoad.AtBound] fact in
 * frames, and a request at or below the current target is a silent no-op
 * (the call is idempotent by design; there is no stale token to misuse). */
sealed class NMPRequestRowsError(message: String) : Exception(message) {
    /** The handle observes the full live set; there is no window to grow. */
    object Unwindowed : NMPRequestRowsError("this observation has no window to grow")

    object EngineClosed : NMPRequestRowsError("engine already shut down")

    /** The canonical store could not serve the advance (staged load rolled
     * back; the window keeps its previous authoritative state). */
    object StoreUnavailable :
        NMPRequestRowsError("window advance could not read or resolve the canonical store")

    /** No planned source could serve the advance (staged load rolled back). */
    data class TransportUnavailable(val reason: String) :
        NMPRequestRowsError("window advance transport unavailable: $reason")

    companion object {
        internal fun from(ffi: FfiRequestRowsException): NMPRequestRowsError =
            when (ffi) {
                is FfiRequestRowsException.Unwindowed -> Unwindowed
                is FfiRequestRowsException.EngineClosed -> EngineClosed
                is FfiRequestRowsException.StoreUnavailable -> StoreUnavailable
                is FfiRequestRowsException.TransportUnavailable ->
                    TransportUnavailable(ffi.reason)
            }
    }
}

/** One windowed observation. [frames] is a single-collector, cold
 * latest-state `Flow`: every delivered [RowBatch] is a full authoritative
 * snapshot of the bounded window (with its [RowBatch.load] growth fact), so
 * a slow collector loses nothing by skipping intermediate frames and never
 * accumulates a backlog (the engine's latest-state mailbox conflates them).
 * Ending collection, calling [cancel], or shutting down the engine withdraws
 * the same observation (the identical collection-scope teardown discipline
 * as the unbounded `observe` flows -- see Query.kt's header for why the
 * `finally`, not a GC `Cleaner`, is the correct JVM mapping). */
class NMPQuery internal constructor(
    private val handle: NmpRowStream,
) {
    private val collectionClaimed = AtomicBoolean(false)
    private val cancelled = AtomicBoolean(false)

    val frames: Flow<RowBatch> =
        flow {
            check(collectionClaimed.compareAndSet(false, true)) {
                "a windowed observation's frames Flow may be collected only once"
            }
            try {
                while (true) {
                    val frame = nmpRethrowingAsync { handle.next() } ?: break
                    emit(windowRowBatch(frame))
                }
            } finally {
                this@NMPQuery.cancel()
            }
        }

    /** Monotonically raise the window's row target to at least [atLeast],
     * clamped to the window's declared `max`. Declarative and idempotent:
     * calling with a value at or below the current target is a no-op, so
     * callers simply state the total they want ("give me at least 200") --
     * there is no continuation token to thread, and no stale-generation
     * failure mode, because the request carries the whole intent. Growth
     * outcomes arrive as [WindowLoad] facts in [frames], never as return
     * values here. Throws a typed [NMPRequestRowsError] only when the
     * advance genuinely cannot be served. */
    fun requestRows(atLeast: ULong) {
        try {
            handle.requestRows(atLeast)
        } catch (error: FfiRequestRowsException) {
            throw NMPRequestRowsError.from(error)
        }
    }

    /** Withdraw the complete windowed observation now. Idempotent. Wakes any
     * parked [frames] pull to its terminal `null`, completing the flow. */
    fun cancel() {
        if (cancelled.compareAndSet(false, true)) {
            handle.cancel()
        }
    }
}

/**
 * Map one windowed [FfiFrame] to its full authoritative [RowBatch].
 * Extracted as a pure function so the wholesale-replace + load-fact mapping
 * is unit-testable without driving a live [NmpRowStream].
 *
 * Windowed frames are authoritative snapshots: row state is REPLACED
 * wholesale from `frame.window.rows` -- there is no delta folding here (the
 * wire ships windowed frames with empty `deltas`; rows never cross the FFI
 * twice), so even a contradictory wire delta is ignored on this arm.
 */
internal fun windowRowBatch(frame: FfiFrame): RowBatch {
    val contents =
        checkNotNull(frame.window) {
            "windowed observation delivered a frame without window contents"
        }
    return RowBatch(
        rows = contents.rows.map { Row.from(it) },
        evidence = AcquisitionEvidence.from(frame.evidence),
        load = WindowLoad.from(contents.load),
    )
}
