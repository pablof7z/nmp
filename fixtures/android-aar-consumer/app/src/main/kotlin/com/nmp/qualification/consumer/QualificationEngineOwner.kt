package com.nmp.qualification.consumer

import com.nmp.sdk.DiagnosticsSnapshot
import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPDemand
import com.nmp.sdk.NMPEngine
import com.nmp.sdk.Receipt
import com.nmp.sdk.RowBatch
import com.nmp.sdk.WriteIntent
import com.nmp.sdk.WriteStatus
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.emitAll
import kotlinx.coroutines.flow.flow
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

enum class QualificationEngineState {
    Open,
    Closing,
    Closed,
}

data class QualificationLifecycleCensus(
    val engineState: QualificationEngineState,
    val engineInstances: Int,
    val rowCollectors: Int,
    val diagnosticsCollectors: Int,
    val receiptCollectors: Int,
    val maxConcurrentRowCollectors: Int,
) {
    val totalCollectors: Int
        get() = rowCollectors + diagnosticsCollectors + receiptCollectors
}

/**
 * One engine lifetime selected by the consuming qualification application.
 *
 * This is fixture/application code, not an NMP adapter or mandatory container.
 * It adds only observable ownership counters around the SDK's cold Flows; each
 * independent collection still opens its own native handle and its `finally`
 * path withdraws that exact registration.
 */
class QualificationEngineOwner(config: NMPConfig) : AutoCloseable {
    private enum class CollectorKind {
        Rows,
        Diagnostics,
        Receipt,
    }

    private sealed interface OwnerPhase {
        data object Open : OwnerPhase
        data class Closing(val completion: CountDownLatch) : OwnerPhase
        data object Closed : OwnerPhase
    }

    private sealed interface CloseDecision {
        data class Close(val completion: CountDownLatch) : CloseDecision
        data class Await(val completion: CountDownLatch) : CloseDecision
        data object AlreadyClosed : CloseDecision
    }

    val instanceId: Long = nextOwnerId.incrementAndGet()

    private val engine = NMPEngine(config)
    private val lock = Any()
    private var phase: OwnerPhase = OwnerPhase.Open
    private var nextCollectorId = 0L
    private val collectors = mutableMapOf<Long, CollectorKind>()
    private var maxConcurrentRows = 0
    private val _census =
        MutableStateFlow(
            QualificationLifecycleCensus(
                engineState = QualificationEngineState.Open,
                engineInstances = 1,
                rowCollectors = 0,
                diagnosticsCollectors = 0,
                receiptCollectors = 0,
                maxConcurrentRowCollectors = 0,
            ),
        )

    val census: StateFlow<QualificationLifecycleCensus> = _census.asStateFlow()

    init {
        registerOwner(instanceId)
    }

    fun observe(demand: NMPDemand): Flow<RowBatch> =
        tracked(CollectorKind.Rows, engine.observe(demand))

    fun observeDiagnostics(): Flow<DiagnosticsSnapshot> =
        tracked(CollectorKind.Diagnostics, engine.observeDiagnostics())

    internal fun rawDiagnostics(): Flow<DiagnosticsSnapshot> = engine.observeDiagnostics()

    fun publish(intent: WriteIntent): Receipt = engine.publish(intent)

    fun observeReceipt(receipt: Receipt): Flow<WriteStatus> =
        tracked(CollectorKind.Receipt, receipt.status)

    override fun close() {
        val decision =
            synchronized(lock) {
                when (val current = phase) {
                    OwnerPhase.Open -> {
                        val completion = CountDownLatch(1)
                        phase = OwnerPhase.Closing(completion)
                        publishCensusLocked()
                        CloseDecision.Close(completion)
                    }
                    is OwnerPhase.Closing -> CloseDecision.Await(current.completion)
                    OwnerPhase.Closed -> CloseDecision.AlreadyClosed
                }
            }

        when (decision) {
            is CloseDecision.Close -> {
                try {
                    engine.close()
                } finally {
                    synchronized(lock) {
                        phase = OwnerPhase.Closed
                        publishCensusLocked()
                    }
                    unregisterOwner(instanceId)
                    decision.completion.countDown()
                }
            }
            is CloseDecision.Await -> check(decision.completion.await(10, TimeUnit.SECONDS)) {
                "concurrent engine close did not finish within 10 seconds"
            }
            CloseDecision.AlreadyClosed -> Unit
        }
    }

    private fun <T> tracked(kind: CollectorKind, upstream: Flow<T>): Flow<T> =
        flow {
            val collectorId = registerCollector(kind)
            try {
                emitAll(upstream)
            } finally {
                unregisterCollector(collectorId)
            }
        }

    private fun registerCollector(kind: CollectorKind): Long =
        synchronized(lock) {
            check(phase is OwnerPhase.Open) { "qualification engine owner is not open" }
            val collectorId = ++nextCollectorId
            collectors[collectorId] = kind
            if (kind == CollectorKind.Rows) {
                maxConcurrentRows =
                    maxOf(maxConcurrentRows, collectors.values.count { it == CollectorKind.Rows })
            }
            publishCensusLocked()
            collectorId
        }

    private fun unregisterCollector(collectorId: Long) {
        synchronized(lock) {
            check(collectors.remove(collectorId) != null) {
                "collector $collectorId was not owned by this engine"
            }
            publishCensusLocked()
        }
    }

    private fun publishCensusLocked() {
        val publicState =
            when (phase) {
                OwnerPhase.Open -> QualificationEngineState.Open
                is OwnerPhase.Closing -> QualificationEngineState.Closing
                OwnerPhase.Closed -> QualificationEngineState.Closed
            }
        _census.value =
            QualificationLifecycleCensus(
                engineState = publicState,
                engineInstances = if (publicState == QualificationEngineState.Closed) 0 else 1,
                rowCollectors = collectors.values.count { it == CollectorKind.Rows },
                diagnosticsCollectors = collectors.values.count { it == CollectorKind.Diagnostics },
                receiptCollectors = collectors.values.count { it == CollectorKind.Receipt },
                maxConcurrentRowCollectors = maxConcurrentRows,
            )
    }

    companion object {
        private val nextOwnerId = AtomicLong(0)
        private val ownerRegistryLock = Any()
        private val liveOwnerIds = mutableSetOf<Long>()
        private val _liveEngineOwners = MutableStateFlow(0)

        val liveEngineOwners: StateFlow<Int> = _liveEngineOwners.asStateFlow()

        fun requireExactlyOneLiveEngineOwner() {
            check(liveEngineOwners.value == 1) {
                "expected exactly one app-owned engine; found ${liveEngineOwners.value}"
            }
        }

        private fun registerOwner(id: Long) {
            synchronized(ownerRegistryLock) {
                check(liveOwnerIds.add(id)) { "engine owner $id registered twice" }
                _liveEngineOwners.value = liveOwnerIds.size
            }
        }

        private fun unregisterOwner(id: Long) {
            synchronized(ownerRegistryLock) {
                check(liveOwnerIds.remove(id)) { "engine owner $id was not live" }
                _liveEngineOwners.value = liveOwnerIds.size
            }
        }
    }
}
