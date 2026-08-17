// The live-query identity, in ergonomic Kotlin shape (#107) -- mirrors
// NMPDemand.swift field-for-field. Every read declares one:
// `NMPEngine.observe` takes an `NMPLiveQuery` whose branches are
// `NMPDemand`s. No door infers routing from the selection's shape (#847) --
// but the routing an app gets by saying nothing is `Auto`, so saying nothing
// is the ordinary way to read.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiAccessContext
import uniffi.nmp_ffi.FfiCacheMode
import uniffi.nmp_ffi.FfiDemand
import uniffi.nmp_ffi.FfiFreshness
import uniffi.nmp_ffi.FfiLiveQuery
import uniffi.nmp_ffi.liveQueryUnion
import uniffi.nmp_ffi.maxQueryBranches
import uniffi.nmp_ffi.FfiReadRouting

/** Where a query's reads come from (`nmp_grammar::ReadRouting` mirror). A
 * strategy, not a resolved relay set: re-executed against whatever the engine
 * knows at each moment.
 *
 * These two words are the whole app-facing routing vocabulary, for reads and
 * writes alike -- see `NMPWriteRouting`. */
sealed class NMPReadRouting {
    /** "Figure out where to read this from." NIP-65 outbound relays for every
     * author the selection resolves, relay hints and prior provenance, then
     * the operator's app and fallback lanes.
     *
     * The default. Naming a routing value is what an app does to OVERRIDE
     * NMP, never what it must do to use NMP. */
    object Auto : NMPReadRouting()

    /** Ask ONLY this relay set, on the wire, full stop -- never widened to
     * outbox, directory, app, fallback or indexer relays, regardless of
     * whether the selection is author-bearing. Must be nonempty:
     * `NMPEngine.observe` throws `NMPError.EmptyExplicitRelaySet` if it is
     * not. */
    data class Explicit(val relays: List<String>) : NMPReadRouting()

    fun toFfi(): FfiReadRouting =
        when (this) {
            is Auto -> FfiReadRouting.Auto
            is Explicit -> FfiReadRouting.Explicit(relays)
        }

    companion object {
        fun from(ffi: FfiReadRouting): NMPReadRouting =
            when (ffi) {
                is FfiReadRouting.Auto -> Auto
                is FfiReadRouting.Explicit -> Explicit(ffi.relays)
            }
    }
}

/** `nmp_grammar::AccessContext` mirror. Closed vocabulary: an unauthenticated
 * [Public] connection, or NIP-42 authentication against one stable expected
 * public key (hex). The [Nip42] identity is frozen in the demand; changing the
 * current account never redirects it (#8). Modelled as a sealed class rather
 * than an enum so the authenticated variant can carry its expected key. */
sealed class NMPAccessContext {
    object Public : NMPAccessContext()

    data class Nip42(val publicKey: String) : NMPAccessContext()

    fun toFfi(): FfiAccessContext =
        when (this) {
            is Public -> FfiAccessContext.Public
            is Nip42 -> FfiAccessContext.Nip42(publicKey)
        }

    companion object {
        fun from(ffi: FfiAccessContext): NMPAccessContext =
            when (ffi) {
                is FfiAccessContext.Public -> Public
                is FfiAccessContext.Nip42 -> Nip42(ffi.publicKey)
            }
    }
}

/** `nmp_grammar::CacheMode` mirror (#107). Meaningful only alongside
 * `NMPReadRouting.Explicit` -- a no-op under `Auto`, since there is no
 * explicit relay set to intersect a cached row's provenance against. */
enum class NMPCacheMode {
    /** Serve every matching cached row regardless of provenance. */
    Agnostic,

    /** Serve only cached rows whose unioned provenance set intersects the
     * explicit relay set. */
    Strict,
    ;

    fun toFfi(): FfiCacheMode =
        when (this) {
            Agnostic -> FfiCacheMode.AGNOSTIC
            Strict -> FfiCacheMode.STRICT
        }

    companion object {
        fun from(ffi: FfiCacheMode): NMPCacheMode =
            when (ffi) {
                FfiCacheMode.AGNOSTIC -> Agnostic
                FfiCacheMode.STRICT -> Strict
            }
    }
}

/** Per-handle coverage/wire policy (`nmp_grammar::Freshness`, #565).
 * Whole seconds match Nostr timestamp and coverage-watermark precision. */
sealed class NMPFreshness {
    object Live : NMPFreshness()
    data class MaxAge(val seconds: ULong) : NMPFreshness()
    object CacheOnly : NMPFreshness()

    fun toFfi(): FfiFreshness =
        when (this) {
            is Live -> FfiFreshness.Live
            is MaxAge -> FfiFreshness.MaxAge(seconds)
            is CacheOnly -> FfiFreshness.CacheOnly
        }

    companion object {
        fun from(ffi: FfiFreshness): NMPFreshness =
            when (ffi) {
                is FfiFreshness.Live -> Live
                is FfiFreshness.MaxAge -> MaxAge(ffi.seconds)
                is FfiFreshness.CacheOnly -> CacheOnly
            }
    }
}

/** The full live-query declaration a dev supplies -- `selection + routing +
 * access + cache + freshness` (`nmp_grammar::Demand` mirror, #106/#107/#565).
 *
 * Every parameter but [selection] defaults, so the ordinary declaration is
 * the selection and nothing else: `NMPDemand(filter)` routes `Auto`. */
data class NMPDemand(
    val selection: NMPFilter,
    /** Where this demand's reads come from. Defaults to [NMPReadRouting.Auto]:
     * an app that says nothing gets NMP's routing. */
    val routing: NMPReadRouting = NMPReadRouting.Auto,
    val access: NMPAccessContext = NMPAccessContext.Public,
    val cache: NMPCacheMode = NMPCacheMode.Agnostic,
    val freshness: NMPFreshness = NMPFreshness.Live,
) {
    fun toFfi(): FfiDemand =
        FfiDemand(
            selection = selection.toFfi(),
            routing = routing.toFfi(),
            access = access.toFfi(),
            cache = cache.toFfi(),
            freshness = freshness.toFfi(),
        )

    companion object {
        fun from(ffi: FfiDemand): NMPDemand =
            NMPDemand(
                selection = NMPFilter.from(ffi.selection),
                routing = NMPReadRouting.from(ffi.routing),
                access = NMPAccessContext.from(ffi.access),
                cache = NMPCacheMode.from(ffi.cache),
                freshness = NMPFreshness.from(ffi.freshness),
            )
    }
}

/**
 * One live-query declaration (#1108): one or more complete, independent
 * [NMPDemand] branches observed through ONE lifecycle, plus the optional
 * bound on their merged row union.
 *
 * Some correct reads need several branches whose results form one semantic
 * query and whose host-scoped values must not cross between them. Flattening
 * two hosts into one `NMPReadRouting.Explicit(listOf(a, b))` produces a
 * confidently wrong cross-product; handing an app a list of demands makes the
 * app own the aggregate observation. This is neither: it is one read noun.
 *
 * A value's branches are always canonical -- sorted, exact duplicates
 * collapsed, never empty -- because [single] and [union] are the only ways to
 * make one, and [union] canonicalizes through the same Rust construction the
 * engine itself uses. Two declarations of the same branches are therefore one
 * value with one hash whatever order they were typed in, [branches] is exactly
 * what the observation will open, and `branches[i]` names the branch each
 * delivered `RowBatch.evidence[i]` reports on.
 */
class NMPLiveQuery private constructor(
    private val canonicalBranches: List<NMPDemand>,
    private val mergedRowBound: UInt?,
) {
    /** The canonical demand branches, in the one order this observation's
     * per-branch evidence is indexed by. Never empty. */
    val branches: List<NMPDemand> get() = canonicalBranches

    /** Bound on the MERGED row union, applied after branch rows are merged by
     * event id -- never `N` rows per branch. Distinct from a branch's own
     * `NMPFilter.limit`, which bounds only that branch's selection. */
    val aggregateResultLimit: UInt? get() = mergedRowBound

    fun toFfi(): FfiLiveQuery =
        FfiLiveQuery(
            branches = canonicalBranches.map { it.toFfi() },
            aggregateResultLimit = mergedRowBound,
        )

    override fun equals(other: Any?): Boolean =
        this === other ||
            (
                other is NMPLiveQuery &&
                    canonicalBranches == other.canonicalBranches &&
                    mergedRowBound == other.mergedRowBound
            )

    override fun hashCode(): Int = 31 * canonicalBranches.hashCode() + mergedRowBound.hashCode()

    override fun toString(): String =
        "NMPLiveQuery(branches=$canonicalBranches, aggregateResultLimit=$mergedRowBound)"

    companion object {
        /** The hard ceiling on branches in one observation. Exceeding it
         * refuses the whole declaration. */
        val MAX_BRANCHES: UInt get() = maxQueryBranches()

        /** One complete demand observed on its own -- already canonical, so
         * this cannot be refused. Identical in lifecycle, frame shape and
         * evidence shape to a union of one. */
        fun single(branch: NMPDemand): NMPLiveQuery = NMPLiveQuery(listOf(branch), null)

        /** Compose independent live queries into ONE canonical declaration.
         *
         * Inputs flatten, duplicates collapse and order is canonicalized, so
         * permutations of the same inputs are the same value.
         * [aggregateResultLimit] bounds the merged row union globally.
         *
         * Throws [NMPError.EmptyQueryUnion] for no branches at all,
         * [NMPError.AggregateResultLimitZero] for a bound that can never
         * contain a row, [NMPError.NestedAggregateResultLimit] when an input
         * already carries its own aggregate bound, and
         * [NMPError.TooManyQueryBranches] above [MAX_BRANCHES] -- the whole
         * declaration is refused, never a silently installed subset. */
        fun union(
            branches: List<NMPLiveQuery>,
            aggregateResultLimit: UInt? = null,
        ): NMPLiveQuery =
            from(nmpRethrowing { liveQueryUnion(branches.map { it.toFfi() }, aggregateResultLimit) })

        /** Lift a live query Rust already built ([union]'s own result, and
         * every protocol-composed read such as `NMPGroup.read`). It is
         * canonical by construction there, so this never re-canonicalizes --
         * and being internal, it is not a door an app can forge a
         * noncanonical value through. */
        internal fun from(ffi: FfiLiveQuery): NMPLiveQuery =
            NMPLiveQuery(
                canonicalBranches = ffi.branches.map { NMPDemand.from(it) },
                mergedRowBound = ffi.aggregateResultLimit,
            )
    }
}
