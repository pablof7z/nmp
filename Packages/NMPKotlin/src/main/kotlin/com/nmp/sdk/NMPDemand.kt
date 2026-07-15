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
import uniffi.nmp_ffi.FfiSourceAuthority

/** Which authority resolves a query's relay set (`nmp_grammar::
 * SourceAuthority` mirror, #107). */
sealed class NMPSourceAuthority {
    object AuthorOutboxes : NMPSourceAuthority()

    object Public : NMPSourceAuthority()

    /** Ask ONLY this relay set, on the wire, full stop -- never author-
     * outbox/directory/app/fallback/indexer routing, regardless of whether
     * the selection is author-bearing. Must be nonempty:
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

/** The full live-query identity a dev declares -- `selection + source +
 * access + cache` (`nmp_grammar::Demand` mirror, #106/#107). */
data class NMPDemand(
    val selection: NMPFilter,
    val source: NMPSourceAuthority,
    val access: NMPAccessContext = NMPAccessContext.Public,
    val cache: NMPCacheMode = NMPCacheMode.Agnostic,
) {
    fun toFfi(): FfiDemand =
        FfiDemand(
            selection = selection.toFfi(),
            source = source.toFfi(),
            access = access.toFfi(),
            cache = cache.toFfi(),
        )

    companion object {
        fun from(ffi: FfiDemand): NMPDemand =
            NMPDemand(
                selection = NMPFilter.from(ffi.selection),
                source = NMPSourceAuthority.from(ffi.source),
                access = NMPAccessContext.from(ffi.access),
                cache = NMPCacheMode.from(ffi.cache),
            )
    }
}
