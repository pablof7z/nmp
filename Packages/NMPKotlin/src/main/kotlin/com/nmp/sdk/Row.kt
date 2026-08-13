// The read noun's delivered value types, in ergonomic Kotlin shape.
// RAW TOKENS ONLY (VISION ledger #12, inherited from `FfiRow`'s own
// contract) -- this layer adds no formatting, no display concept
// whatsoever; that stays app-owned. Mirrors Row.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiAuthPhase
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowSignature
import uniffi.nmp_ffi.FfiShortfallFact
import uniffi.nmp_ffi.FfiSourceEvidence
import uniffi.nmp_ffi.FfiSourceStatus

/** The one owner of a row's signature property. NMP-emitted Signed rows have
 * passed verification; raw app-constructed values make no validity claim. */
sealed class RowSignature {
    object Pending : RowSignature()

    data class Signed(val signature: String) : RowSignature()

    companion object {
        fun from(ffi: FfiRowSignature): RowSignature =
            when (ffi) {
                FfiRowSignature.Pending -> Pending
                is FfiRowSignature.Signed -> Signed(ffi.signature)
            }
    }

    internal fun toFfi(): FfiRowSignature =
        when (this) {
            Pending -> FfiRowSignature.Pending
            is Signed -> FfiRowSignature.Signed(signature)
        }
}

/** One delivered event, verbatim. */
data class Row(
    val id: String,
    val pubkey: String,
    val createdAt: ULong,
    val kind: UShort,
    /** Each inner list is one raw tag (`["p", "<hex>", ...]`), verbatim. */
    val tags: List<List<String>>,
    val content: String,
    /** Pending carries no signature; Signed necessarily carries one. */
    val signature: RowSignature,
    /**
     * Sorted, deduplicated relay URLs that have delivered this event id
     * (#105) -- raw tokens, not a formatted/display field either.
     */
    val sources: List<String>,
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
                signature = RowSignature.from(ffi.signature),
                sources = ffi.sources,
            )
    }
}

/** Closed AUTH negotiation phase vocabulary shared by evidence and
 * diagnostics (populated by the #8 AUTH reducer). */
sealed class AuthPhase {
    object AwaitingChallenge : AuthPhase()

    object AwaitingPolicy : AuthPhase()

    object AwaitingSignature : AuthPhase()

    object AwaitingRelayAck : AuthPhase()

    object Ready : AuthPhase()

    object Denied : AuthPhase()

    object Error : AuthPhase()

    companion object {
        fun from(ffi: FfiAuthPhase): AuthPhase =
            when (ffi) {
                FfiAuthPhase.AWAITING_CHALLENGE -> AwaitingChallenge
                FfiAuthPhase.AWAITING_POLICY -> AwaitingPolicy
                FfiAuthPhase.AWAITING_SIGNATURE -> AwaitingSignature
                FfiAuthPhase.AWAITING_RELAY_ACK -> AwaitingRelayAck
                FfiAuthPhase.READY -> Ready
                FfiAuthPhase.DENIED -> Denied
                FfiAuthPhase.ERROR -> Error
            }
    }
}

/** The closed, honest per-source vocabulary
 * (`docs/design/scoped-evidence-49-12-plan.md` §4). */
sealed class SourceStatus {
    object Requesting : SourceStatus()

    /** This relay reached NIP-01's end of stored events for every request
     * covering this query on this session: it sent everything it had for the
     * question it was asked (#1235). A delivery fact about ONE source
     * answering ONE request -- never a claim that the query is complete, that
     * another source finished, or that anything was proven over the query's
     * window. What was proven is `reconciledThrough`, and the two disagree in
     * both directions. */
    object FinishedStoredEvents : SourceStatus()

    /** The request is planned and locally owned, but the transport has not
     * accepted it yet. This includes bounded retry backoff after a local
     * admission refusal; the relay socket may already be connected. */
    object AwaitingRequest : SourceStatus()
    object CoverageSatisfied : SourceStatus()

    object Connecting : SourceStatus()

    object Disconnected : SourceStatus()

    data class AwaitingAuth(val phase: AuthPhase) : SourceStatus()

    object AuthDenied : SourceStatus()

    /** This exact source cannot currently acquire its planned work: worker
     * open, request placement, or AUTH failed. Never a global sync verdict. */
    object Error : SourceStatus()

    companion object {
        fun from(ffi: FfiSourceStatus): SourceStatus =
            when (ffi) {
                is FfiSourceStatus.Requesting -> Requesting
                is FfiSourceStatus.FinishedStoredEvents -> FinishedStoredEvents
                is FfiSourceStatus.AwaitingRequest -> AwaitingRequest
                is FfiSourceStatus.CoverageSatisfied -> CoverageSatisfied
                is FfiSourceStatus.Connecting -> Connecting
                is FfiSourceStatus.Disconnected -> Disconnected
                is FfiSourceStatus.AwaitingAuth -> AwaitingAuth(AuthPhase.from(ffi.phase))
                is FfiSourceStatus.AuthDenied -> AuthDenied
                is FfiSourceStatus.Error -> Error
            }
    }
}

/** One relay's acquisition state for a query's subtree, as two deliberately
 * orthogonal facts: a durable PAST fact (`reconciledThrough`) and its effective
 * current acquisition state (`status`). A relay can be currently
 * `Disconnected` while still carrying a perfectly good `reconciledThrough`
 * from before it dropped, and `FinishedStoredEvents` while carrying none at
 * all. A fresh no-wire scope is `CoverageSatisfied` independently of link
 * state. */
data class SourceEvidence(
    val relay: String,
    /** The frozen access identity of the physical session that produced this
     * per-source fact (#8): the same relay URL under [NMPAccessContext.Public]
     * versus a [NMPAccessContext.Nip42] identity is a distinct, non-aliasing
     * source. */
    val access: NMPAccessContext,
    val reconciledThrough: ULong?,
    val status: SourceStatus,
) {
    companion object {
        fun from(ffi: FfiSourceEvidence): SourceEvidence =
            SourceEvidence(
                relay = ffi.relay,
                access = NMPAccessContext.from(ffi.access),
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
 * NOT a query-level verdict -- an app reads which source
 * has proven what and rolls that into its own progress policy; NMP never
 * does that rollup for it. */
data class AcquisitionEvidence(
    val sources: List<SourceEvidence>,
    val shortfall: List<ShortfallFact>,
) {
    companion object {
        fun from(ffi: FfiAcquisitionEvidence): AcquisitionEvidence =
            AcquisitionEvidence(
                sources = ffi.sources.map { SourceEvidence.from(it) },
                shortfall = ffi.shortfall.map { ShortfallFact.from(it) },
            )
    }
}

/** One delivered read-noun element: the full row snapshot (never a bare
 * delta -- unbounded observations accumulate deltas in the Query.kt bridge;
 * windowed observations deliver authoritative snapshots directly, see
 * Window.kt) plus the query's current scoped acquisition evidence. */
data class RowBatch(
    val rows: List<Row>,
    /** This observation's acquisition evidence, ONE entry per canonical query
     * branch in branch order (#1108). A single-branch live query carries
     * exactly one entry. Branch identity is never erased: two branches that
     * resolved the same value keep separate entries, and one branch's
     * shortfall is never masked by a sibling's proof. */
    val evidence: List<AcquisitionEvidence>,
    /** Mechanical growth state of the observation's expandable window --
     * a fact, never a completeness claim. `null` iff the observation is
     * unbounded (no window, hence no growth state to report). */
    val load: WindowLoad? = null,
)
