package com.nmp.sdk

import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import uniffi.nmp_ffi.FfiContentClaimDecision
import uniffi.nmp_ffi.FfiContentHydrationPolicy
import uniffi.nmp_ffi.FfiContentResolutionDecision
import uniffi.nmp_ffi.evaluateContentClaim
import uniffi.nmp_ffi.evaluateContentResolution

@ConsistentCopyVisibility
data class NostrContentPolicy private constructor(
    val maxActiveReferences: Int,
    val maxResolvedReferences: Int,
    val maxDepth: Int,
    val releaseGraceMilliseconds: Long,
) {
    internal fun toFfi() =
        FfiContentHydrationPolicy(
            maxActiveReferences.toUInt(),
            maxResolvedReferences.toUInt(),
            maxDepth.coerceAtMost(UByte.MAX_VALUE.toInt()).toUByte(),
        )

    companion object {
        /**
         * Mirrors Swift's [NostrContentPolicy] contract: invalid bounds are
         * silently clamped to the nearest valid value rather than throwing.
         */
        operator fun invoke(
            maxActiveReferences: Int = 24,
            maxResolvedReferences: Int = 96,
            maxDepth: Int = 3,
            releaseGraceMilliseconds: Long = 350,
        ): NostrContentPolicy =
            NostrContentPolicy(
                maxActiveReferences.coerceAtLeast(1),
                maxResolvedReferences.coerceAtLeast(1),
                maxDepth.coerceAtLeast(0),
                releaseGraceMilliseconds.coerceAtLeast(0),
            )
    }
}

data class NostrContentRenderContext(
    val path: List<String> = emptyList(),
    val depth: Int = 0,
) {
    fun descending(targetKey: String) =
        NostrContentRenderContext(path + targetKey, depth + 1)
}

data class NostrContentEvidence(
    val canonical: AcquisitionEvidence? = null,
    val helpers: List<AcquisitionEvidence> = emptyList(),
)

sealed class NostrContentShortfall {
    object NoPlannedSource : NostrContentShortfall()

    data class QueryRejected(val message: String) : NostrContentShortfall()

    object InvalidResolvedRow : NostrContentShortfall()
}

sealed class NostrContentCollapseReason {
    data class Cycle(val targetKey: String) : NostrContentCollapseReason()

    data class Depth(val maximum: Int) : NostrContentCollapseReason()

    data class ActiveBudget(val maximum: Int) : NostrContentCollapseReason()

    data class ResolvedBudget(val maximum: Int) : NostrContentCollapseReason()
}

sealed class NostrContentResource {
    abstract val event: Row

    data class Profile(val metadata: NostrProfileMetadata, override val event: Row) :
        NostrContentResource()

    data class Event(override val event: Row) : NostrContentResource()

    val profile: NostrProfileMetadata?
        get() = (this as? Profile)?.metadata

    val article: NostrArticle?
        get() = decodeNip23Article(event)
}

sealed class NostrReferenceState {
    object Idle : NostrReferenceState()

    data class Loading(val evidence: NostrContentEvidence) : NostrReferenceState()

    data class Refreshing(
        val cached: NostrContentResource,
        val evidence: NostrContentEvidence,
    ) : NostrReferenceState()

    data class Resolved(
        val value: NostrContentResource,
        val evidence: NostrContentEvidence,
    ) : NostrReferenceState()

    data class Withdrawn(
        val previous: NostrContentResource,
        val evidence: NostrContentEvidence,
    ) : NostrReferenceState()

    data class Shortfall(
        val reason: NostrContentShortfall,
        val evidence: NostrContentEvidence,
    ) : NostrReferenceState()

    data class Stopped(val evidence: NostrContentEvidence) : NostrReferenceState()

    data class Collapsed(val reason: NostrContentCollapseReason) : NostrReferenceState()

    val resource: NostrContentResource?
        get() =
            when (this) {
                is Refreshing -> cached
                is Resolved -> value
                else -> null
            }
}

data class NostrContentSnapshot(
    val document: NostrContentDocument,
    val nodes: Map<ULong, NostrReferenceState>,
    val resources: Map<String, NostrReferenceState>,
    val revision: ULong,
    val activeReferenceCount: Int,
) {
    fun state(occurrence: NostrReferenceOccurrence): NostrReferenceState =
        nodes[occurrence.id] ?: NostrReferenceState.Idle

    fun state(target: NostrReferenceTarget): NostrReferenceState =
        resources[target.key] ?: NostrReferenceState.Idle
}

class NMPContentClient(private val engine: NMPEngine) {
    fun session(
        content: String,
        scope: CoroutineScope,
        syntax: NostrContentSyntax = NostrContentSyntax.PlainText,
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = NostrContentRenderContext(),
    ): NostrContentSession =
        session(parseNostrContent(content, syntax), scope, policy, context)

    fun session(
        document: NostrContentDocument,
        scope: CoroutineScope,
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = NostrContentRenderContext(),
    ): NostrContentSession = NostrContentSession(engine, document, scope, policy, context)
}

class NostrContentClaim internal constructor(private val release: () -> Unit) : AutoCloseable {
    private val closed = AtomicBoolean(false)

    override fun close() {
        if (closed.compareAndSet(false, true)) release()
    }
}

class NostrContentSession internal constructor(
    private val engine: NMPEngine,
    document: NostrContentDocument,
    parentScope: CoroutineScope,
    val policy: NostrContentPolicy,
    val context: NostrContentRenderContext,
) : AutoCloseable {
    private data class TargetPlan(
        val target: NostrReferenceTarget,
        val canonical: NMPDemand,
        val helpers: MutableList<NMPDemand>,
        val occurrenceIds: MutableSet<ULong>,
    )

    private val lock = ReentrantLock()
    private val sessionJob = SupervisorJob(parentScope.coroutineContext[Job])
    private val scope = CoroutineScope(parentScope.coroutineContext + sessionJob)
    private val plans = mutableMapOf<String, TargetPlan>()
    private val targetForOccurrence = mutableMapOf<ULong, String>()
    private val states = mutableMapOf<String, NostrReferenceState>()
    private val claimCounts = mutableMapOf<String, Int>()
    private val activeTargets = mutableSetOf<String>()
    private val waitingTargets = mutableSetOf<String>()
    private val resolvedTargets = mutableSetOf<String>()
    private val canonicalEvidence = mutableMapOf<String, AcquisitionEvidence>()
    private val helperEvidence = mutableMapOf<String, MutableMap<Int, AcquisitionEvidence>>()
    private val observationJobs = mutableMapOf<String, MutableList<Job>>()
    private val releaseJobs = mutableMapOf<String, Job>()
    private var revision = 0UL
    private val _snapshot =
        MutableStateFlow(
            NostrContentSnapshot(document, emptyMap(), emptyMap(), 0UL, 0),
        )

    val snapshot: StateFlow<NostrContentSnapshot> = _snapshot.asStateFlow()

    init {
        lock.withLock {
            document.references.forEach(::addOccurrenceLocked)
            publishLocked()
        }
    }

    fun claim(referenceId: ULong): NostrContentClaim? =
        lock.withLock {
            targetForOccurrence[referenceId]?.let(::claimTargetLocked)
        }

    fun claim(target: NostrReferenceTarget): NostrContentClaim =
        lock.withLock {
            claimTargetLocked(ensurePlanLocked(target))
        }

    fun claimProfile(pubkey: String): NostrContentClaim =
        claim(NostrReferenceTarget.Profile(pubkey))

    fun state(target: NostrReferenceTarget): NostrReferenceState =
        lock.withLock { states[target.key] ?: NostrReferenceState.Idle }

    override fun close() {
        lock.withLock {
            releaseJobs.values.forEach(Job::cancel)
            releaseJobs.clear()
            observationJobs.values.flatten().forEach(Job::cancel)
            observationJobs.clear()
            activeTargets.clear()
            waitingTargets.clear()
            publishLocked()
        }
        sessionJob.cancel()
    }

    /**
     * Deterministically withdraw every content-derived demand now.
     *
     * Unlike [close], this leaves the session alive and reusable: [scope] and
     * [sessionJob] keep running, so a later [claim] on a target re-arms
     * observation for it.
     */
    fun stop() {
        lock.withLock {
            releaseJobs.values.forEach(Job::cancel)
            releaseJobs.clear()
            observationJobs.values.flatten().forEach(Job::cancel)
            observationJobs.clear()
            activeTargets.clear()
            waitingTargets.clear()
            for (key in states.keys.toList()) {
                when (val state = states[key] ?: continue) {
                    is NostrReferenceState.Loading, is NostrReferenceState.Shortfall,
                    is NostrReferenceState.Stopped, is NostrReferenceState.Withdrawn ->
                        states[key] = NostrReferenceState.Idle
                    is NostrReferenceState.Refreshing ->
                        states[key] = NostrReferenceState.Resolved(state.cached, state.evidence)
                    is NostrReferenceState.Idle, is NostrReferenceState.Resolved,
                    is NostrReferenceState.Collapsed -> Unit
                }
            }
            publishLocked()
        }
    }

    private fun addOccurrenceLocked(occurrence: NostrReferenceOccurrence) {
        val plan = referenceDemandPlan(occurrence.target)
        targetForOccurrence[occurrence.id] = plan.targetKey
        val existing = plans[plan.targetKey]
        if (existing != null) {
            existing.helpers.addAll(plan.helpers.filterNot(existing.helpers::contains))
            existing.occurrenceIds.add(occurrence.id)
        } else {
            plans[plan.targetKey] =
                TargetPlan(
                    occurrence.target,
                    plan.canonical,
                    plan.helpers.toMutableList(),
                    mutableSetOf(occurrence.id),
                )
            states[plan.targetKey] = NostrReferenceState.Idle
        }
    }

    private fun ensurePlanLocked(target: NostrReferenceTarget): String {
        val plan = referenceDemandPlan(target)
        val existing = plans[plan.targetKey]
        if (existing != null) {
            existing.helpers.addAll(plan.helpers.filterNot(existing.helpers::contains))
        } else {
            plans[plan.targetKey] =
                TargetPlan(target, plan.canonical, plan.helpers.toMutableList(), mutableSetOf())
            states[plan.targetKey] = NostrReferenceState.Idle
            publishLocked()
        }
        return plan.targetKey
    }

    private fun claimTargetLocked(key: String): NostrContentClaim {
        releaseJobs.remove(key)?.cancel()
        claimCounts[key] = (claimCounts[key] ?: 0) + 1
        if (claimCounts[key] == 1) startIfPossibleLocked(key)
        return NostrContentClaim {
            scope.launch { release(key) }
        }
    }

    private fun release(key: String) {
        lock.withLock {
            val remaining = ((claimCounts[key] ?: 0) - 1).coerceAtLeast(0)
            claimCounts[key] = remaining
            if (remaining != 0) return
            releaseJobs[key] =
                scope.launch {
                    if (policy.releaseGraceMilliseconds > 0) {
                        delay(policy.releaseGraceMilliseconds)
                    }
                    finishRelease(key)
                }
        }
    }

    private fun finishRelease(key: String) {
        lock.withLock {
            if (claimCounts[key] != 0) return
            releaseJobs.remove(key)
            waitingTargets.remove(key)
            stopObservingLocked(key, preserveResolved = true)
            startWaitingLocked()
        }
    }

    private fun startIfPossibleLocked(key: String) {
        if (!plans.containsKey(key)) return
        when (
            val decision =
                evaluateContentClaim(
                    key,
                    context.path,
                    context.depth.coerceAtMost(UByte.MAX_VALUE.toInt()).toUByte(),
                    activeTargets.size.toUInt(),
                    policy.toFfi(),
                )
        ) {
            is FfiContentClaimDecision.Acquire -> startObservingLocked(key)
            is FfiContentClaimDecision.Cycle -> {
                states[key] =
                    NostrReferenceState.Collapsed(
                        NostrContentCollapseReason.Cycle(decision.targetKey),
                    )
                publishLocked()
            }
            is FfiContentClaimDecision.DepthLimit -> {
                states[key] =
                    NostrReferenceState.Collapsed(
                        NostrContentCollapseReason.Depth(decision.maximum.toInt()),
                    )
                publishLocked()
            }
            is FfiContentClaimDecision.ActiveLimit -> {
                waitingTargets.add(key)
                states[key] =
                    NostrReferenceState.Collapsed(
                        NostrContentCollapseReason.ActiveBudget(decision.maximum.toInt()),
                    )
                publishLocked()
            }
        }
    }

    private fun startWaitingLocked() {
        for (key in waitingTargets.sorted()) {
            if (activeTargets.size >= policy.maxActiveReferences) break
            if ((claimCounts[key] ?: 0) == 0) {
                waitingTargets.remove(key)
                continue
            }
            waitingTargets.remove(key)
            startIfPossibleLocked(key)
        }
    }

    private fun startObservingLocked(key: String) {
        if (!activeTargets.add(key)) return
        val plan = plans.getValue(key)
        states[key] =
            states[key]?.resource?.let {
                NostrReferenceState.Refreshing(it, evidenceLocked(key))
            } ?: NostrReferenceState.Loading(evidenceLocked(key))
        publishLocked()

        val jobs = mutableListOf<Job>()
        jobs +=
            scope.launch {
                try {
                    engine.observe(plan.canonical).collect { receiveCanonical(key, it) }
                    canonicalStopped(key)
                } catch (_: CancellationException) {
                    // Collection-scope cancellation is normal demand withdrawal.
                } catch (error: Throwable) {
                    canonicalFailed(key, error)
                }
            }
        plan.helpers.forEachIndexed { index, demand ->
            jobs +=
                scope.launch {
                    try {
                        engine.observe(demand).collect { receiveHelper(key, index, it) }
                    } catch (_: CancellationException) {
                        // Normal teardown.
                    }
                }
        }
        observationJobs[key] = jobs
    }

    private fun receiveCanonical(key: String, batch: RowBatch) {
        lock.withLock {
            if (!activeTargets.contains(key)) return
            canonicalEvidence[key] = batch.evidence
            val row = batch.rows.firstOrNull()
            if (row == null) {
                val previous = states[key]?.resource
                states[key] =
                    when {
                        previous != null ->
                            NostrReferenceState.Withdrawn(previous, evidenceLocked(key))
                        batch.evidence.shortfall.isEmpty() ->
                            NostrReferenceState.Loading(evidenceLocked(key))
                        else ->
                            NostrReferenceState.Shortfall(
                                NostrContentShortfall.NoPlannedSource,
                                evidenceLocked(key),
                            )
                    }
                publishLocked()
                return
            }

            when (
                val decision =
                    evaluateContentResolution(
                        resolvedTargets.contains(key),
                        resolvedTargets.size.toUInt(),
                        policy.toFfi(),
                    )
            ) {
                is FfiContentResolutionDecision.ResolvedLimit -> {
                    states[key] =
                        NostrReferenceState.Collapsed(
                            NostrContentCollapseReason.ResolvedBudget(decision.maximum.toInt()),
                        )
                    stopObservingLocked(key, preserveResolved = false)
                    startWaitingLocked()
                    return
                }
                is FfiContentResolutionDecision.Accept -> Unit
            }

            val resource =
                when (plans.getValue(key).target) {
                    is NostrReferenceTarget.Profile ->
                        decodeNostrProfile(row)?.let { NostrContentResource.Profile(it, row) }
                    is NostrReferenceTarget.Event,
                    is NostrReferenceTarget.Address,
                    -> NostrContentResource.Event(row)
                }
            if (resource == null) {
                states[key] =
                    NostrReferenceState.Shortfall(
                        NostrContentShortfall.InvalidResolvedRow,
                        evidenceLocked(key),
                    )
            } else {
                resolvedTargets.add(key)
                states[key] = NostrReferenceState.Resolved(resource, evidenceLocked(key))
            }
            publishLocked()
        }
    }

    private fun receiveHelper(key: String, index: Int, batch: RowBatch) {
        lock.withLock {
            helperEvidence.getOrPut(key, ::mutableMapOf)[index] = batch.evidence
            val resource = states[key]?.resource
            states[key] =
                when {
                    resource != null && states[key] is NostrReferenceState.Refreshing ->
                        NostrReferenceState.Refreshing(resource, evidenceLocked(key))
                    resource != null -> NostrReferenceState.Resolved(resource, evidenceLocked(key))
                    batch.evidence.shortfall.isEmpty() ->
                        NostrReferenceState.Loading(evidenceLocked(key))
                    else ->
                        NostrReferenceState.Shortfall(
                            NostrContentShortfall.NoPlannedSource,
                            evidenceLocked(key),
                        )
                }
            publishLocked()
        }
    }

    private fun canonicalStopped(key: String) {
        lock.withLock {
            if (activeTargets.contains(key) && states[key]?.resource == null) {
                states[key] = NostrReferenceState.Stopped(evidenceLocked(key))
                publishLocked()
            }
        }
    }

    private fun canonicalFailed(key: String, error: Throwable) {
        lock.withLock {
            if (!activeTargets.remove(key)) return
            observationJobs.remove(key)?.forEach(Job::cancel)
            states[key] =
                NostrReferenceState.Shortfall(
                    NostrContentShortfall.QueryRejected(error.toString()),
                    evidenceLocked(key),
                )
            publishLocked()
            startWaitingLocked()
        }
    }

    private fun stopObservingLocked(key: String, preserveResolved: Boolean) {
        observationJobs.remove(key)?.forEach(Job::cancel)
        activeTargets.remove(key)
        val state = states[key]
        states[key] =
            when {
                preserveResolved && state is NostrReferenceState.Refreshing ->
                    NostrReferenceState.Resolved(state.cached, state.evidence)
                preserveResolved && state?.resource != null -> state
                else -> NostrReferenceState.Idle
            }
        publishLocked()
    }

    private fun evidenceLocked(key: String) =
        NostrContentEvidence(
            canonicalEvidence[key],
            helperEvidence[key].orEmpty().toSortedMap().values.toList(),
        )

    private fun publishLocked() {
        revision += 1UL
        val nodes =
            targetForOccurrence.mapValues { (_, key) ->
                states[key] ?: NostrReferenceState.Idle
            }
        _snapshot.value =
            NostrContentSnapshot(
                _snapshot.value.document,
                nodes,
                states.toMap(),
                revision,
                activeTargets.size,
            )
    }
}
