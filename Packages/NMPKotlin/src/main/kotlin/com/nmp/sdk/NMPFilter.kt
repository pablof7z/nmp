// The two-noun query descriptor, in ergonomic Kotlin shape -- mirrors
// NMPFilter.swift field-for-field. A dev builds an `NMPFilter` value --
// never an `FfiFilter`/`FfiBinding` directly -- and hands it to
// `NMPEngine.observe(_)`. `NMPBinding` mirrors `nmp_grammar::Binding`'s
// four cases as a plain sealed class; unlike Swift's `indirect enum`,
// Kotlin's sealed classes are already heap-allocated references, so no
// `indirect` keyword is needed for `.Derived` to self-embed a complete
// `NMPDemand`. `FfiDerived`/`FfiSetOp` (UniFFI objects, needed only because
// Rust enums can't self-reference) are constructed on the way OUT
// (`toFfi()`), never exposed to a caller.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiBinding
import uniffi.nmp_ffi.FfiDerived
import uniffi.nmp_ffi.FfiFilter
import uniffi.nmp_ffi.FfiIdentityField
import uniffi.nmp_ffi.FfiSelector
import uniffi.nmp_ffi.FfiSetAlgebra
import uniffi.nmp_ffi.FfiSetOp

/** The reactive identity root (VISION P3). `ActivePubkey` is the only
 * variant today; additional identity fields are additive. */
enum class NMPIdentityField {
    ActivePubkey,
    ;

    fun toFfi(): FfiIdentityField =
        when (this) {
            ActivePubkey -> FfiIdentityField.ACTIVE_PUBKEY
        }

    companion object {
        fun from(ffi: FfiIdentityField): NMPIdentityField =
            when (ffi) {
                FfiIdentityField.ACTIVE_PUBKEY -> ActivePubkey
            }
    }
}

/** The closed projection vocabulary a `.Derived` binding projects through. */
sealed class NMPSelector {
    object Authors : NMPSelector()

    object Ids : NMPSelector()

    /** An arbitrary event-tag key (#64) -- projects already-acquired events
     * locally, so it is NOT restricted to `NMPFilter.tags`' single-letter
     * wire alphabet. `"-"`, `"poop"`, `"alt"`, or any other
     * multi-character/punctuation tag name an event actually carries is a
     * legal key here; case and spelling are matched exactly. */
    data class Tag(val name: String) : NMPSelector()

    object AddressCoord : NMPSelector()

    fun toFfi(): FfiSelector =
        when (this) {
            is Authors -> FfiSelector.Authors
            is Ids -> FfiSelector.Ids
            is Tag -> FfiSelector.Tag(name)
            is AddressCoord -> FfiSelector.AddressCoord
        }

    companion object {
        fun from(ffi: FfiSelector): NMPSelector =
            when (ffi) {
                is FfiSelector.Authors -> Authors
                is FfiSelector.Ids -> Ids
                is FfiSelector.Tag -> Tag(ffi.name)
                is FfiSelector.AddressCoord -> AddressCoord
            }
    }
}

/** Set algebra over resolved value sets (union/intersect/diff of two or
 * more bindings -- e.g. "follows minus mutes"). */
enum class NMPSetAlgebra {
    Union,
    Intersect,
    Diff,
    ;

    fun toFfi(): FfiSetAlgebra =
        when (this) {
            Union -> FfiSetAlgebra.UNION
            Intersect -> FfiSetAlgebra.INTERSECT
            Diff -> FfiSetAlgebra.DIFF
        }

    companion object {
        fun from(ffi: FfiSetAlgebra): NMPSetAlgebra =
            when (ffi) {
                FfiSetAlgebra.UNION -> Union
                FfiSetAlgebra.INTERSECT -> Intersect
                FfiSetAlgebra.DIFF -> Diff
            }
    }
}

/** Every bindable filter-field value. */
sealed class NMPBinding {
    /** A fixed, literal set of hex values (pubkeys/ids/tag values). */
    data class Literal(val values: Set<String>) : NMPBinding()

    /** Re-resolves reactively whenever the named identity field changes
     * (e.g. the active account). */
    data class Reactive(val field: NMPIdentityField) : NMPBinding()

    /** Projects `inner`'s matching rows through `project` (e.g. "authors of
     * my kind:3 contact list, projected through their `p` tags" = follows).
     * The inner demand owns source, access, cache, and freshness independently
     * from the outer demand. */
    data class Derived(val inner: NMPDemand, val project: NMPSelector) : NMPBinding()

    /** Combines several bindings with a set operation. */
    data class SetOp(val op: NMPSetAlgebra, val operands: List<NMPBinding>) : NMPBinding()

    fun toFfi(): FfiBinding =
        when (this) {
            is Literal -> FfiBinding.Literal(values.toList())
            is Reactive -> FfiBinding.Reactive(field.toFfi())
            is Derived -> FfiBinding.Derived(FfiDerived(inner.toFfi(), project.toFfi()))
            is SetOp -> FfiBinding.SetOp(FfiSetOp(op.toFfi(), operands.map { it.toFfi() }))
        }

    companion object {
        fun from(ffi: FfiBinding): NMPBinding =
            when (ffi) {
                is FfiBinding.Literal -> Literal(ffi.values.toSet())
                is FfiBinding.Reactive -> Reactive(NMPIdentityField.from(ffi.field))
                is FfiBinding.Derived ->
                    Derived(NMPDemand.from(ffi.derived.inner()), NMPSelector.from(ffi.derived.project()))
                is FfiBinding.SetOp ->
                    SetOp(NMPSetAlgebra.from(ffi.setOp.op()), ffi.setOp.operands().map { from(it) })
            }
    }
}

/** A live-query filter whose field values may be reactive `NMPBinding`s
 * (`nmp_grammar::Filter` mirror, Kotlin-shaped). Values, not code: an
 * `NMPFilter` is a plain `data class` and re-`observe`-able at will --
 * editing a filter is constructing a new value, never mutating a running
 * query in place. */
data class NMPFilter(
    val kinds: List<UShort>? = null,
    val authors: NMPBinding? = null,
    val ids: NMPBinding? = null,
    val tags: Map<Char, NMPBinding> = emptyMap(),
    val since: ULong? = null,
    val until: ULong? = null,
    val limit: UInt? = null,
) {
    fun toFfi(): FfiFilter =
        FfiFilter(
            kinds = kinds,
            authors = authors?.toFfi(),
            ids = ids?.toFfi(),
            tags = tags.mapKeys { it.key.toString() }.mapValues { it.value.toFfi() },
            since = since,
            until = until,
            limit = limit,
        )

    companion object {
        fun from(ffi: FfiFilter): NMPFilter =
            NMPFilter(
                kinds = ffi.kinds,
                authors = ffi.authors?.let { NMPBinding.from(it) },
                ids = ffi.ids?.let { NMPBinding.from(it) },
                tags = ffi.tags.mapKeys { it.key.firstOrNull() ?: ' ' }.mapValues { NMPBinding.from(it.value) },
                since = ffi.since,
                until = ffi.until,
                limit = ffi.limit,
            )
    }
}
