// The read noun's delivered value types, in ergonomic Kotlin shape.
// RAW TOKENS ONLY (VISION ledger #12, inherited from `FfiRow`'s own
// contract) -- this layer adds no formatting, no display concept
// whatsoever; that stays app-owned. Mirrors Row.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiAuthPhase
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiShortfallFact
import uniffi.nmp_ffi.FfiSourceEvidence
import uniffi.nmp_ffi.FfiSourceStatus

/** One delivered event, verbatim. */
data class Row(
    val id: String,
    val pubkey: String,
    val createdAt: ULong,
    val kind: UShort,
    /** Each inner list is one raw tag (`["p", "<hex>", ...]`), verbatim. */
    val tags: List<List<String>>,
    val content: String,
    val sig: String,
) {
    companion object {
        fun from(ffi: FfiRow): Row =
            Row(
                id = ffi.id,
                pubkey = ffi.pubkey,
                createdAt = ffi.createdAt,
                kind = ffi.kind,
                tags = ffi.tags,
                content = ffi.content,
                sig = ffi.sig,
            )
    }
}

/** The AUTH negotiation phases worth surfacing while awaiting proof
 * (reserved for #8 -- not yet populated by the engine). */
sealed class AuthPhase {
    object AwaitingPolicy : AuthPhase()

    object AwaitingSignature : AuthPhase()

    companion object {
        fun from(ffi: FfiAuthPhase): AuthPhase =
            when (ffi) {
                is FfiAuthPhase.AwaitingPolicy -> AwaitingPolicy
                is FfiAuthPhase.AwaitingSignature -> AwaitingSignature
            }
    }
}

/** The closed, honest per-source link-status vocabulary
 * (`docs/design/scoped-evidence-49-12-plan.md` §4). */
sealed class SourceStatus {
    object Requesting : SourceStatus()

    object Connecting : SourceStatus()

    object Disconnected : SourceStatus()

    data class AwaitingAuth(val phase: AuthPhase) : SourceStatus()

    object AuthDenied : SourceStatus()

    object Error : SourceStatus()

    companion object {
        fun from(ffi: FfiSourceStatus): SourceStatus =
            when (ffi) {
                is FfiSourceStatus.Requesting -> Requesting
                is FfiSourceStatus.Connecting -> Connecting
                is FfiSourceStatus.Disconnected -> Disconnected
                is FfiSourceStatus.AwaitingAuth -> AwaitingAuth(AuthPhase.from(ffi.phase))
                is FfiSourceStatus.AuthDenied -> AuthDenied
                is FfiSourceStatus.Error -> Error
            }
    }
}

/** One relay's acquisition state for a query's subtree, as two deliberately
 * orthogonal facts: a durable PAST fact (`reconciledThrough`) and a current
 * LINK fact (`status`) -- a relay can be currently `Disconnected` while
 * still carrying a perfectly good `reconciledThrough` from before it
 * dropped (offline cached rows remain usable). */
data class SourceEvidence(
    val relay: String,
    val reconciledThrough: ULong?,
    val status: SourceStatus,
) {
    companion object {
        fun from(ffi: FfiSourceEvidence): SourceEvidence =
            SourceEvidence(
                relay = ffi.relay,
                reconciledThrough = ffi.reconciledThrough,
                status = SourceStatus.from(ffi.status),
            )
    }
}

/** An explicit, never-silent shortfall in a query's subtree acquisition --
 * facts about what nothing is (yet) trying to acquire, never folded into
 * `AcquisitionEvidence.sources`. `atom` is the exact wire JSON of the
 * unacquired filter shape. */
sealed class ShortfallFact {
    data class NoPlannedSource(val atom: String) : ShortfallFact()

    object NoResolvedDemand : ShortfallFact()

    data class LocalLimit(val atom: String) : ShortfallFact()

    companion object {
        fun from(ffi: FfiShortfallFact): ShortfallFact =
            when (ffi) {
                is FfiShortfallFact.NoPlannedSource -> NoPlannedSource(ffi.atom)
                is FfiShortfallFact.NoResolvedDemand -> NoResolvedDemand
                is FfiShortfallFact.LocalLimit -> LocalLimit(ffi.atom)
            }
    }
}

/** A query's scoped acquisition evidence
 * (`docs/design/scoped-evidence-49-12-plan.md` §4): per-source facts over
 * the query's full subtree, plus an explicit shortfall list. Deliberately
 * NOT a completeness/sync/isComplete verdict -- an app reads which source
 * has proven what and rolls that into its own progress policy; NMP never
 * does that rollup for it. */
data class AcquisitionEvidence(
    val sources: List<SourceEvidence> = emptyList(),
    val shortfall: List<ShortfallFact> = emptyList(),
) {
    companion object {
        fun from(ffi: FfiAcquisitionEvidence): AcquisitionEvidence =
            AcquisitionEvidence(
                sources = ffi.sources.map { SourceEvidence.from(it) },
                shortfall = ffi.shortfall.map { ShortfallFact.from(it) },
            )
    }
}

/** One `NMPQuery` element: the full accumulated snapshot (never a bare
 * delta -- the `Flow` bridge does the accumulation, see Query.kt) plus the
 * query's current scoped acquisition evidence. */
data class RowBatch(val rows: List<Row>, val evidence: AcquisitionEvidence)
