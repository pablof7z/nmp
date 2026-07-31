// The explicit live-query identity, in ergonomic Kotlin shape (#107) --
// mirrors NMPDemand.swift field-for-field. `NMPEngine.observe(NMPFilter)`
// still applies the static AuthorOutboxes/Public default
// (`nmp_grammar::Demand::from_filter`); a dev reaches for `NMPDemand` once
// that default isn't enough -- declaring `Pinned` wire authority or a
// non-`Agnostic` cache mode.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiAccessContext
import uniffi.nmp_ffi.FfiCacheMode
import uniffi.nmp_ffi.FfiDemand
import uniffi.nmp_ffi.FfiFreshness
import uniffi.nmp_ffi.FfiLiveQuery
import uniffi.nmp_ffi.maxQueryBranches
import uniffi.nmp_ffi.FfiSourceAuthority

/** Which authority resolves a query's relay set (`nmp_grammar::
 * SourceAuthority` mirror, #107). */
sealed class NMPSourceAuthority {
    object AuthorOutboxes : NMPSourceAuthority()

    object Public : NMPSourceAuthority()

    /** Ask ONLY this relay set, on the wire, full stop -- never neutral
     * author facts, hints, provenance, or operator policy, regardless of
     * whether the selection is author-bearing. Must be nonempty:
     * `NMPEngine.observe(NMPDemand)` throws
     * `NMPError.EmptyPinnedRelaySet` if it is not. */
    data class Pinned(val relays: Set<String>) : NMPSourceAuthority()

    fun toFfi(): FfiSourceAuthority =
        when (this) {
            is AuthorOutboxes -> FfiSourceAuthority.AuthorOutboxes
            is Public -> FfiSourceAuthority.Public
            is Pinned -> FfiSourceAuthority.Pinned(relays.toList())
        }

    companion object {
        fun from(ffi: FfiSourceAuthority): NMPSourceAuthority =
            when (ffi) {
                is FfiSourceAuthority.AuthorOutboxes -> AuthorOutboxes
                is FfiSourceAuthority.Public -> Public
                is FfiSourceAuthority.Pinned -> Pinned(ffi.relays.toSet())
            }
    }
}

/** `nmp_grammar::AccessContext` mirror. Closed vocabulary: an unauthenticated
 * [Public] connection, or NIP-42 authentication against one stable expected
 * public key (hex). The [Nip42] identity is frozen in the demand; changing the
 * active account never redirects it (#8). Modelled as a sealed class rather
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
 * `NMPSourceAuthority.Pinned` -- a no-op under any other source, since
 * there is no pinned relay set to intersect a cached row's provenance
 * against. */
enum class NMPCacheMode {
    /** Serve every matching cached row regardless of provenance. */
    Agnostic,

    /** Serve only cached rows whose unioned provenance set intersects the
     * pinned relay set. */
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

/** The full live-query declaration a dev supplies -- `selection + source +
 * access + cache + freshness` (`nmp_grammar::Demand` mirror, #106/#107/#565). */
data class NMPDemand(
    val selection: NMPFilter,
    val source: NMPSourceAuthority,
    val access: NMPAccessContext = NMPAccessContext.Public,
    val cache: NMPCacheMode = NMPCacheMode.Agnostic,
    val freshness: NMPFreshness = NMPFreshness.Live,
) {
    fun toFfi(): FfiDemand =
        FfiDemand(
            selection = selection.toFfi(),
            source = source.toFfi(),
            access = access.toFfi(),
            cache = cache.toFfi(),
            freshness = freshness.toFfi(),
        )

    companion object {
        fun from(ffi: FfiDemand): NMPDemand =
            NMPDemand(
                selection = NMPFilter.from(ffi.selection),
                source = NMPSourceAuthority.from(ffi.source),
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
 * two hosts into one `NMPSourceAuthority.Pinned(setOf(a, b))` produces a
 * confidently wrong cross-product; handing an app a list of demands makes the
 * app own the aggregate observation. This is neither: it is one read noun.
 *
 * The Rust constructor canonicalizes the branches -- sorted, exact duplicates
 * collapsed -- so permuted or repeated input yields the same observation and
 * the same per-branch evidence order.
 */
data class NMPLiveQuery(
    /** The demand branches. Must be nonempty and at most [MAX_BRANCHES];
     * both are typed refusals at `observe`, never silent truncation. */
    val branches: List<NMPDemand>,
    /** Bound on the MERGED row union, applied after branch rows are merged by
     * event id -- never `N` rows per branch. Distinct from a branch's own
     * `NMPFilter.limit`, which bounds only that branch's selection. */
    val aggregateResultLimit: UInt? = null,
) {
    fun toFfi(): FfiLiveQuery =
        FfiLiveQuery(
            branches = branches.map { it.toFfi() },
            aggregateResultLimit = aggregateResultLimit,
        )

    companion object {
        /** The hard ceiling on branches in one observation. Exceeding it
         * refuses the whole declaration. */
        val MAX_BRANCHES: UInt get() = maxQueryBranches()

        fun from(ffi: FfiLiveQuery): NMPLiveQuery =
            NMPLiveQuery(
                branches = ffi.branches.map { NMPDemand.from(it) },
                aggregateResultLimit = ffi.aggregateResultLimit,
            )
    }
}
