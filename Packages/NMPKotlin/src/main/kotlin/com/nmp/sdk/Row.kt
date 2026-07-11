// The read noun's delivered value types, in ergonomic Kotlin shape.
// RAW TOKENS ONLY (VISION ledger #12, inherited from `FfiRow`'s own
// contract) -- this layer adds no formatting, no display concept
// whatsoever; that stays app-owned. Mirrors Row.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiCoverage
import uniffi.nmp_ffi.FfiRow

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

/** A query's aggregate coverage (ledger #7's variant): whether the engine
 * can PROVE the visible rows are everything up to a point in time, or
 * whether that has not (yet) been established. */
sealed class Coverage {
    data class CompleteUpTo(val unixSeconds: ULong) : Coverage()

    object Unknown : Coverage()

    companion object {
        fun from(ffi: FfiCoverage): Coverage =
            when (ffi) {
                is FfiCoverage.CompleteUpTo -> CompleteUpTo(ffi.unixSeconds)
                is FfiCoverage.Unknown -> Unknown
            }
    }
}

/** One `NMPQuery` element: the full accumulated snapshot (never a bare
 * delta -- the `Flow` bridge does the accumulation, see Query.kt) plus the
 * query's current coverage. */
data class RowBatch(val rows: List<Row>, val coverage: Coverage)
