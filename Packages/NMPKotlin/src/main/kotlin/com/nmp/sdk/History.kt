package com.nmp.sdk

import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import uniffi.nmp_ffi.FfiHistoryBatch
import uniffi.nmp_ffi.FfiHistoryLoadException
import uniffi.nmp_ffi.FfiHistoryLoadFact
import uniffi.nmp_ffi.FfiHistoryQuery
import uniffi.nmp_ffi.FfiRowDelta
import uniffi.nmp_ffi.HistoryObserver
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpHistoryContinuation
import uniffi.nmp_ffi.NmpHistoryHandle
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

/** Mechanical state of the most recent local history advance. It is never a
 * claim that the network is complete or that no older event exists. */
sealed class HistoryLoadFact {
    object Idle : HistoryLoadFact()
    object Requesting : HistoryLoadFact()
    data class Returned(val added: ULong) : HistoryLoadFact()
    data class AtBound(val maxRows: ULong) : HistoryLoadFact()

    companion object {
        internal fun from(ffi: FfiHistoryLoadFact): HistoryLoadFact =
            when (ffi) {
                is FfiHistoryLoadFact.Idle -> Idle
                is FfiHistoryLoadFact.Requesting -> Requesting
                is FfiHistoryLoadFact.Returned -> Returned(ffi.added)
                is FfiHistoryLoadFact.AtBound -> AtBound(ffi.maxRows)
            }
    }
}

/** Opaque process-local capability for one exact history generation. There
 * is deliberately no public constructor or native-field access. */
class NMPHistoryContinuation internal constructor(
    internal val ffi: NmpHistoryContinuation,
)

/** One self-contained bounded history state. [rows] is authoritative and is
 * already in canonical `createdAt DESC, id ASC` order. */
data class HistoryBatch(
    val rows: List<Row>,
    val continuation: NMPHistoryContinuation?,
    val evidence: AcquisitionEvidence,
    val load: HistoryLoadFact,
)

/** Exact failures from [NMPHistoryQuery.loadOlder]. Continuation misuse,
 * local-store failure, and acquisition failure stay distinct. */
sealed class NMPHistoryLoadError(message: String) : Exception(message) {
    object WrongVersion : NMPHistoryLoadError("history continuation version is unsupported")
    object WrongEngine : NMPHistoryLoadError("history continuation belongs to another engine")
    object WrongSession : NMPHistoryLoadError("history continuation belongs to another session")
    object WrongDescriptor :
        NMPHistoryLoadError("history continuation belongs to another demand descriptor")
    object StaleGeneration : NMPHistoryLoadError("history continuation is stale")
    object LoadInProgress : NMPHistoryLoadError("history session already has a staged load")
    data class AtBound(val maxRows: ULong) :
        NMPHistoryLoadError("history session is at its maxRows bound $maxRows")
    object NoBoundary : NMPHistoryLoadError("history session has no row boundary to advance")
    object StoreUnavailable :
        NMPHistoryLoadError("history advance could not read or resolve the canonical store")
    data class TransportUnavailable(val reason: String) :
        NMPHistoryLoadError("history advance transport unavailable: $reason")

    companion object {
        internal fun from(ffi: FfiHistoryLoadException): NMPHistoryLoadError =
            when (ffi) {
                is FfiHistoryLoadException.WrongVersion -> WrongVersion
                is FfiHistoryLoadException.WrongEngine -> WrongEngine
                is FfiHistoryLoadException.WrongSession -> WrongSession
                is FfiHistoryLoadException.WrongDescriptor -> WrongDescriptor
                is FfiHistoryLoadException.StaleGeneration -> StaleGeneration
                is FfiHistoryLoadException.LoadInProgress -> LoadInProgress
                is FfiHistoryLoadException.AtBound -> AtBound(ffi.maxRows)
                is FfiHistoryLoadException.NoBoundary -> NoBoundary
                is FfiHistoryLoadException.StoreUnavailable -> StoreUnavailable
                is FfiHistoryLoadException.TransportUnavailable ->
                    TransportUnavailable(ffi.reason)
            }
    }
}

/** One coordinated bounded-history session. [batches] is a single-collector,
 * latest-state `Flow`: every delivered element is a full authoritative state,
 * and a slow collector never creates a growing backlog. Ending collection,
 * calling [cancel], or shutting down the engine withdraws the same session. */
class NMPHistoryQuery internal constructor(
    engine: NmpEngineInterface,
    demand: NMPDemand,
    pageSize: ULong,
    maxRows: ULong,
) {
    private val bridge = HistoryBridge(maxRows)
    private val handle: NmpHistoryHandle =
        nmpRethrowing {
            engine.observeHistory(
                FfiHistoryQuery(
                    demand = demand.toFfi(),
                    pageSize = pageSize,
                    maxRows = maxRows,
                ),
                bridge,
            )
        }
    private val cancelled = AtomicBoolean(false)

    val batches: Flow<HistoryBatch> =
        callbackFlow {
            bridge.attach(
                emit = { trySend(it) },
                finish = { close() },
            )
            awaitClose {
                bridge.detach()
                this@NMPHistoryQuery.cancel()
            }
        }.conflate()

    /** Advance this session using only the latest continuation it issued. */
    fun loadOlder(continuation: NMPHistoryContinuation) {
        try {
            handle.loadOlder(continuation.ffi)
        } catch (error: FfiHistoryLoadException) {
            throw NMPHistoryLoadError.from(error)
        }
    }

    /** Withdraw the complete history session now. Idempotent. */
    fun cancel() {
        if (cancelled.compareAndSet(false, true)) {
            handle.cancel()
            bridge.finish()
        }
    }
}

/** The only callback implementation in the ergonomic Kotlin layer. Every
 * receiver-relative delta is reduced immediately and checked against the
 * same frame's authoritative bounded rows; only that full state is retained. */
internal class HistoryBridge(private val maxRows: ULong) : HistoryObserver {
    private val lock = ReentrantLock()
    private var priorById = emptyMap<String, Row>()
    private var latest: HistoryBatch? = null
    private var emit: ((HistoryBatch) -> Unit)? = null
    private var finishSink: (() -> Unit)? = null
    private var collectionClaimed = false
    private var closed = false

    override fun onBatch(batch: FfiHistoryBatch) {
        val outgoing =
            lock.withLock {
                if (closed) return
                val mapped = reduceHistoryFrame(priorById, batch, maxRows)
                priorById = mapped.rows.associateBy(Row::id)
                val sink = emit
                if (sink == null) {
                    latest = mapped
                    null
                } else {
                    sink to mapped
                }
            }
        outgoing?.first?.invoke(outgoing.second)
    }

    override fun onClosed() = finish()

    fun attach(emit: (HistoryBatch) -> Unit, finish: () -> Unit) {
        val alreadyClosed =
            lock.withLock {
                check(!collectionClaimed) {
                    "a history session's batches Flow may be collected only once"
                }
                collectionClaimed = true
                this.emit = emit
                this.finishSink = finish
                val pending = latest
                latest = null
                // `emit` is the callbackFlow's non-blocking `trySend` seam.
                // Sending while holding the bridge lock prevents onClosed
                // from winning the attach race and discarding this latest
                // already-produced authoritative frame.
                pending?.let(emit)
                closed
            }
        if (alreadyClosed) finish()
    }

    fun detach() {
        lock.withLock {
            emit = null
            finishSink = null
        }
    }

    fun finish() {
        val closer =
            lock.withLock {
                if (closed) return
                closed = true
                finishSink
            }
        closer?.invoke()
    }
}

/** Pure receiver-side reducer used by the bridge and its bounded-state test. */
internal fun reduceHistoryFrame(
    priorById: Map<String, Row>,
    batch: FfiHistoryBatch,
    maxRows: ULong,
): HistoryBatch {
    val authoritative = batch.rows.map { Row.from(it) }
    check(authoritative.size.toULong() <= maxRows) {
        "Rust history frame exceeded its declared maxRows bound"
    }
    val authoritativeById = authoritative.associateBy(Row::id)
    check(authoritativeById.size == authoritative.size) {
        "Rust history frame contains duplicate row ids"
    }

    val reduced = priorById.toMutableMap()
    for (delta in batch.deltas) {
        when (delta) {
            is FfiRowDelta.Added -> {
                val row = Row.from(delta.row)
                reduced[row.id] = row
            }
            is FfiRowDelta.SourcesGrew -> {
                reduced[delta.id]?.let { row ->
                    reduced[delta.id] = row.copy(sources = delta.sources)
                }
            }
            is FfiRowDelta.Removed -> reduced.remove(delta.id)
        }
    }
    check(reduced == authoritativeById) {
        "history deltas must describe the authoritative full frame"
    }

    return HistoryBatch(
        rows = authoritative,
        continuation = batch.continuation?.let(::NMPHistoryContinuation),
        evidence = AcquisitionEvidence.from(batch.evidence),
        load = HistoryLoadFact.from(batch.load),
    )
}
